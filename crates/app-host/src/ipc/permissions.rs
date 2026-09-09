//! Permission IPC: approve/deny/list, standing grants.
use super::Dispatcher;
use capability_core::{Capability, PrincipalKind, Resource, ResourceScope};
use ipc_core::{ErrorCode, IpcErrorBody, IpcRequest};
use policy_core::{GrantLifetime, QueueError};
use serde_json::Value;

impl Dispatcher {
    /// Resolve one permission reply from the dialog. Approving records a
    /// grant scoped to the requested capability + resource, so the resumed
    /// turn authorizes through normal policy — never a bypass.
    pub(crate) fn decide(
        &self,
        request: &IpcRequest,
        approve: bool,
    ) -> Result<Value, IpcErrorBody> {
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let id = request
            .params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "permission reply needs a string 'id'".to_string(),
            })?;
        let lifetime = if approve {
            Some(parse_lifetime(
                request.params.get("lifetime").and_then(|v| v.as_str()),
            )?)
        } else {
            None
        };
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        let id_owned = id.to_string();
        let outcome = queue.decide(id, lifetime);
        // Release the queue lock before waking the runtime: the resumed
        // worker re-locks the queue for its policy context.
        drop(queue);
        match outcome {
            Ok(granted) => {
                // Wake a suspended turn, if this decision resolves it.
                // Approve resumes under the new grant; deny resumes with
                // a refusal note so the model works around it.
                if let Some(agent) = self.agent.as_ref() {
                    agent.notify_decided(&id_owned, granted);
                }
                Ok(serde_json::json!({ "ok": true, "granted": granted }))
            }
            Err(QueueError::UnknownId(_)) => Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "unknown permission request".to_string(),
            }),
            // Storage failed: the request is still pending, so the user
            // can retry. Internal, not a params problem.
            Err(QueueError::Persist(detail)) => Err(IpcErrorBody {
                code: ErrorCode::Internal,
                message: format!("persistent approval could not be stored: {detail}"),
            }),
        }
    }
    /// Snapshot of queued requests so a (re)mounted dialog can sync
    /// without racing boot-time push events.
    pub(crate) fn list_pending(&self) -> Result<Value, IpcErrorBody> {
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        serde_json::to_value(queue.list()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })
    }
    /// The user's home directory, resolved from the environment. Used as
    /// the default broad-read scope when settings omit a path.
    pub(crate) fn home_dir() -> Result<std::path::PathBuf, IpcErrorBody> {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .map_err(|_| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "cannot determine the home directory; pass an explicit path".to_string(),
            })
    }
    /// Resolve the grant/revoke target: an explicit absolute path, or the
    /// home directory. The target must exist (canonicalized, so `..` and
    /// symlinks resolve before any check) and must not itself be a secret
    /// path — key directories keep per-file approval even from their owner.
    pub(crate) fn grant_root(params: &Value) -> Result<std::path::PathBuf, IpcErrorBody> {
        let raw = params.get("path").and_then(|v| v.as_str());
        let base = match raw {
            Some(p) => std::path::PathBuf::from(p),
            None => Self::home_dir()?,
        };
        if !base.is_absolute() {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "grant path must be absolute".to_string(),
            });
        }
        let canonical = base.canonicalize().map_err(|_| IpcErrorBody {
            code: ErrorCode::InvalidParams,
            message: format!("grant path does not exist: {}", base.display()),
        })?;
        if policy_core::is_secret_path(&Resource::Path(canonical.clone())) {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "secret paths keep per-file approval and cannot be granted in bulk"
                    .to_string(),
            });
        }
        Ok(canonical)
    }
    /// Mint a standing grant from explicit user action (settings UI).
    /// Restricted to `FilesystemRead`: reads can be pre-approved, but
    /// mutations and control in the normal policy go through the per-request
    /// dialog. Autonomous Agent authorization is a separate runtime mode,
    /// never a bulk grant through this endpoint.
    pub(crate) fn permission_grant(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        if request.params.get("capability").and_then(|v| v.as_str()) != Some("FilesystemRead") {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "permission.grant only issues FilesystemRead grants".to_string(),
            });
        }
        // Standing grants outlive the request by definition: only
        // session and persistent lifetimes make sense here. Omitted
        // lifetimes default to session (vanishes on restart); the
        // settings toggle passes persistent explicitly.
        let lifetime = match request.params.get("lifetime").and_then(|v| v.as_str()) {
            None => GrantLifetime::Session,
            Some(_) => {
                let parsed =
                    parse_lifetime(request.params.get("lifetime").and_then(|v| v.as_str()))?;
                if !matches!(parsed, GrantLifetime::Session | GrantLifetime::Persistent) {
                    return Err(IpcErrorBody {
                        code: ErrorCode::InvalidParams,
                        message: "grant lifetime must be 'session' or 'persistent'".to_string(),
                    });
                }
                parsed
            }
        };
        let root = Self::grant_root(&request.params)?;
        let scope = ResourceScope::new(vec![Resource::Path(root.clone())]);
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        queue
            .grant_direct(
                PrincipalKind::Agent,
                Capability::FilesystemRead,
                scope,
                lifetime,
                format!("user broad-read grant for {}", root.display()),
            )
            .map_err(|e| match e {
                QueueError::Persist(detail) => IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: format!("grant could not be stored: {detail}"),
                },
                other => IpcErrorBody {
                    code: ErrorCode::Internal,
                    message: other.to_string(),
                },
            })?;
        Ok(serde_json::json!({ "ok": true, "path": root.to_string_lossy() }))
    }
    /// Drop standing grants for a capability + scope, in memory and in
    /// storage, so revocation takes effect immediately and survives restart.
    pub(crate) fn permission_revoke(&self, request: &IpcRequest) -> Result<Value, IpcErrorBody> {
        if request.params.get("capability").and_then(|v| v.as_str()) != Some("FilesystemRead") {
            return Err(IpcErrorBody {
                code: ErrorCode::InvalidParams,
                message: "permission.revoke only revokes FilesystemRead grants".to_string(),
            });
        }
        let root = Self::grant_root(&request.params)?;
        let scope = ResourceScope::new(vec![Resource::Path(root.clone())]);
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        let mut removed = queue.revoke_where(&Capability::FilesystemRead, &scope);
        drop(queue);
        if let Some(storage) = self.storage.as_ref() {
            let storage = storage.lock().map_err(|_| IpcErrorBody {
                code: ErrorCode::Internal,
                message: "storage lock failed".to_string(),
            })?;
            let rows = storage.load_grants().map_err(|e| IpcErrorBody {
                code: ErrorCode::Internal,
                message: format!("could not load grants: {e}"),
            })?;
            for row in rows {
                if row.grant.capability == Capability::FilesystemRead && row.grant.scope == scope {
                    storage.delete_grant(row.id).map_err(|e| IpcErrorBody {
                        code: ErrorCode::Internal,
                        message: format!("could not delete grant: {e}"),
                    })?;
                    removed += 1;
                }
            }
        }
        Ok(serde_json::json!({ "ok": true, "removed": removed }))
    }
    /// Standing grants plus the resolved home directory, so settings UI
    /// can render the broad-read toggle without guessing paths.
    pub(crate) fn permission_grants(&self) -> Result<Value, IpcErrorBody> {
        let queue = self.approvals.as_ref().ok_or_else(|| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue is not attached".to_string(),
        })?;
        let queue = queue.lock().map_err(|_| IpcErrorBody {
            code: ErrorCode::Internal,
            message: "approval queue lock failed".to_string(),
        })?;
        let grants = serde_json::to_value(queue.grants_snapshot()).map_err(|err| IpcErrorBody {
            code: ErrorCode::Internal,
            message: err.to_string(),
        })?;
        drop(queue);
        let home = Self::home_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        Ok(serde_json::json!({ "grants": grants, "home": home }))
    }
}
/// Dialog lifetime strings → [`GrantLifetime`]. Defaults to `Once` so a
/// dialog that omits duration grants the least authority.
fn parse_lifetime(raw: Option<&str>) -> Result<GrantLifetime, IpcErrorBody> {
    match raw {
        None | Some("once") => Ok(GrantLifetime::Once),
        Some("task") => Ok(GrantLifetime::Task),
        Some("session") => Ok(GrantLifetime::Session),
        Some("persistent") => Ok(GrantLifetime::Persistent),
        Some(other) => Err(IpcErrorBody {
            code: ErrorCode::InvalidParams,
            message: format!("unknown grant lifetime: '{other}'"),
        }),
    }
}
