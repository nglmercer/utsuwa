//! Turn authorization against the approval queue.
use agent_core::ToolAuthorizer;
use policy_core::{ApprovalQueue, AuthorizationDecision};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub(crate) struct QueueAuthorizer {
    pub(crate) approvals: Arc<Mutex<ApprovalQueue>>,
    pub(crate) task_id: String,
    pub(crate) turn_id: String,
    pub(crate) autonomous_full_access: Arc<AtomicBool>,
}

impl ToolAuthorizer for QueueAuthorizer {
    fn authorize(
        &self,
        principal: &capability_core::Principal,
        request: &capability_core::CapabilityRequest,
    ) -> AuthorizationDecision {
        // Autonomous mode is deliberately an AgentRuntime concern. It is
        // checked before the ordinary queue policy so secret paths and
        // shell/interpreter requests are included, but only when both the
        // caller and the request carry the exact trusted Agent identity.
        if self.autonomous_for(principal, request) {
            return AuthorizationDecision::Allow {
                ticket_ttl: policy_core::ticket_ttl_for(&request.capability),
            };
        }
        self.approvals
            .lock()
            .map(|queue| {
                queue.authorize_for(
                    principal,
                    request,
                    Some(self.task_id.clone()),
                    Some(self.turn_id.clone()),
                )
            })
            .unwrap_or_else(|_| AuthorizationDecision::Deny {
                reason: "approval queue lock failed".to_string(),
            })
    }

    fn commit(
        &self,
        principal: &capability_core::Principal,
        request: &capability_core::CapabilityRequest,
    ) -> bool {
        // `Agent::execute_prepared` commits immediately before minting the
        // ticket. Autonomous mode must commit here too; it still mints the
        // same invocation-bound ticket and the broker still validates it.
        if self.autonomous_for(principal, request) {
            return true;
        }
        self.approvals
            .lock()
            .map(|queue| {
                queue.consume_for(principal, request, Some(&self.task_id), Some(&self.turn_id))
            })
            .unwrap_or(false)
    }

    fn authorization_mode(&self) -> Option<&'static str> {
        self.autonomous_full_access
            .load(Ordering::SeqCst)
            .then_some("autonomous_full_access")
    }
}

impl QueueAuthorizer {
    fn autonomous_for(
        &self,
        principal: &capability_core::Principal,
        request: &capability_core::CapabilityRequest,
    ) -> bool {
        self.autonomous_full_access.load(Ordering::SeqCst)
            && matches!(principal, capability_core::Principal::Agent(_))
            && principal == &request.principal
    }
}
