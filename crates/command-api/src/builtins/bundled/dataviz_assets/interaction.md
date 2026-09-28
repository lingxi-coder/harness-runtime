# Interaction

An HTML/SVG chart should be inspectable without guesswork. Add a crosshair and
tooltip to line and area charts, and a per-mark tooltip to bars, dots, and
cells. Keep the hit target larger than the visible mark, and expose the same
value in a table view so hover is never the only way to inspect data.

Put date-range and dimension filters in one row above the chart. Preserve
series identity when filters change, keep keyboard focus visible, and make
tooltips available from the focused mark as well as the pointer. A tooltip
should name the series, the dimension, and the formatted value.
