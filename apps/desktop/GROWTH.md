# Devtools growth

## 2026-07-26

- A room-first desktop shell needs immediate visual truth as part of its
  feedback loop. Entrance opacity on the primary pinned room made valid content
  appear absent during initial paint, so primary navigation and room state now
  render without a reveal delay.
- Fresh Tauri repositories need an icon before `generate_context!` can compile,
  even when application bundling is disabled.
- Native network timeouts and structured error redaction are part of GUI
  operability. A connection attempt that can hang or surface raw transport text
  is not an actionable interface.
