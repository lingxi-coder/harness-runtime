# Reference palette

This is a deliberately brand-neutral starting point. Replace these values with
the host design system's ramps, then run the palette validator in the skill
before shipping a chart.

| Role | Slots |
| --- | --- |
| Categorical | `#2563EB`, `#D97706`, `#059669`, `#DB2777`, `#7C3AED`, `#0891B2` |
| Sequential | `#DBEAFE` → `#1D4ED8` |
| Diverging | `#B91C1C` → `#E5E7EB` → `#1D4ED8` |
| Status | good `#15803D`, warning `#A16207`, serious `#C2410C`, critical `#B91C1C` |
| Surfaces | light `#FFFFFF`, dark `#111827` |

Categorical hues have a fixed order and are never cycled. Sequential scales use
one hue from light to dark; diverging scales use two poles and a neutral middle.
Status colors stay reserved for status and are paired with an icon and label.
