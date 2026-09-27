# Desktop profile

Use when the local app explicitly targets desktop.

- Design for resizable windows, higher information density, hover, right-click
  or secondary actions when relevant, keyboard-first traversal, and stable
  content width.
- Use the host system UI font unless a font asset is already checked in.
- Choose toolbar/sidebar/split-pane patterns intentionally instead of reusing a
  phone tab shell.
- Specify empty space strategy, panel resizing, and long-list behavior.

Avoid:

- merely enlarging the tablet profile;
- touch-only interaction assumptions;
- oversized mobile spacing that wastes desktop canvas.

## Sources

Reviewed: 2026-08-27

- Apple HIG: https://developer.apple.com/design/human-interface-guidelines
- Android adaptive apps: https://developer.android.com/develop/adaptive-apps/guides/get-started-with-adaptive-apps
- WAI keyboard interface: https://www.w3.org/WAI/ARIA/apg/practices/keyboard-interface/
- Windows navigation basics: https://learn.microsoft.com/en-us/windows/apps/design/basics/navigation-basics
