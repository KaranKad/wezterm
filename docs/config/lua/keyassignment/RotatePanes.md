# RotatePanes

*Since: nightly builds only*

Rotates the sequence of panes within the active tab, preserving the sizes based on the tab positions.

Panes within a tab have an ordering that follows the creation order of the splits.

As an example, if you have three panes created in sequence using horizontal
splits, their indices from left to right are `0, 1, 2`:

```
|--------|----|----|
|   0    |  1 |  2 |
|--------|----|----|
```

If you perform a clockwise rotation on that tab, the indices are rearranged
so that the panes are now `2, 0, 1`.

```
|--------|----|----|
|   2    |  0 |  1 |
|--------|----|----|
```

If you instead perform a counter-clockwise rotation then the indices are rearranged
so that the panes are now `1, 2, 0`

```
|--------|----|----|
|   1    |  2 |  0 |
|--------|----|----|
```

The sizes of original positions are preserved; as you can see from the examples
above, the left-most pane is still the largest of the panes despite rotating
the panes withing those placements.

```lua
local wezterm = require 'wezterm'

return {
  keys = {
    {key="b", mods="CTRL", action=wezterm.action{RotatePanes="CounterClockwise"}},
    {key="n", mods="CTRL", action=wezterm.action{RotatePanes="Clockwise"}},
  },
}
```

See also [PaneSelect](PaneSelect.md).
