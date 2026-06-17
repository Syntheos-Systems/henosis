//! Multi-model role routing.
//!
//! Routes LLM calls to different models based on turn number.
//! Turn 1 (planning) uses the primary model; subsequent turns
//! (tool execution loops) use the fast model when configured.

/// Routes model selection across turns.
///
/// When `fast` is set, turn 1 uses `primary` (planning needs strong
/// reasoning) and turn 2+ uses `fast` (tool loops are mechanical).
/// When `fast` is None, `primary` is used for all turns.
#[derive(Debug, Clone)]
pub struct ModelRouter {
    /// Strong model used for the first turn (planning).
    pub primary: String,
    /// Cheaper/faster model used for tool-execution turns.
    pub fast: Option<String>,
}

impl ModelRouter {
    /// Create a router that always uses the given model.
    pub fn new(primary: String) -> Self {
        Self {
            primary,
            fast: None,
        }
    }

    /// Set the fast model for turn 2+.
    pub fn with_fast(mut self, model: String) -> Self {
        self.fast = Some(model);
        self
    }

    /// Select which model to use for the given turn number.
    ///
    /// Turn 1 = primary (needs to plan). Turn 2+ = fast if configured.
    /// Turn 0 is treated the same as turn 1 (defensive).
    pub fn select(&self, turn: usize) -> &str {
        if turn <= 1 || self.fast.is_none() {
            &self.primary
        } else {
            self.fast.as_deref().unwrap_or(&self.primary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_only() {
        let r = ModelRouter::new("opus".into());
        assert_eq!(r.select(0), "opus");
        assert_eq!(r.select(1), "opus");
        assert_eq!(r.select(2), "opus");
        assert_eq!(r.select(100), "opus");
    }

    #[test]
    fn with_fast_model() {
        let r = ModelRouter::new("opus".into()).with_fast("haiku".into());
        assert_eq!(r.select(0), "opus");
        assert_eq!(r.select(1), "opus");
        assert_eq!(r.select(2), "haiku");
        assert_eq!(r.select(3), "haiku");
        assert_eq!(r.select(100), "haiku");
    }
}
