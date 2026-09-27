# iOS and iPadOS profile

After loading `lingxi-local-app:apple-design` in this role execution, use its
Apple Design guidance as the default for confirmed iOS/iPadOS UI output. Keep
the result compact and adapt it to the Host/Ionic shell: use the platform
system font, Ionic navigation/back behavior, device safe areas, and the
reserved 80-by-80 CSS-pixel bottom-leading host control corner. An explicit
brand or reference in the confirmed contract takes precedence over this
default. This profile routes output; it does not repeat the Apple skill's full
motion or material tutorial.

- iPhone: favor hierarchical navigation, native-feeling top bars and tab bars,
  44 CSS-pixel targets, safe areas, Dynamic Type tolerance, swipe-back, sheets and
  popovers where appropriate.
- iPad: use sidebar or split/list-detail presentation, controlled content width,
  portrait and landscape behavior, pointer hover/focus, and keyboard shortcuts
  when the flow benefits from them.
- If both iPhone and iPad are targeted, write different `presentation` entries;
  do not stretch the phone shell.
- In a mixed target matrix, apply these defaults to iPhone/iPad outputs only;
  route Android and desktop through their own profiles.
- For a canvas app, apply Apple treatment to the HUD/menu overlay and its
  safe-area placement only; never redesign the drawn scene as DOM chrome.

Avoid:

- Material FAB/ripple patterns;
- phone-only single columns on iPad when the brief needs browsing/detail work;
- ignoring pointer and hardware keyboard behavior on iPad.

## Sources

Reviewed: 2026-08-27

- Apple HIG: https://developer.apple.com/design/human-interface-guidelines
- Tab bars: https://developer.apple.com/design/human-interface-guidelines/tab-bars
- Toolbars: https://developer.apple.com/design/human-interface-guidelines/toolbars
- Search fields: https://developer.apple.com/design/human-interface-guidelines/search-fields
