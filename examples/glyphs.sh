#!/usr/bin/env bash
# A glyph gallery for shellglass. Run it under the viewer to see every glyph range
# the renderer draws specially — box-drawing, block elements, legacy-computing
# mosaics and powerline separators — rendered from the font with no local install:
#
#     shellglass serve -- ./examples/glyphs.sh
#
# then open the printed URL. Press Enter in the terminal to quit.
set -u

# Print one codepoint (hex number) as its UTF-8 glyph.
glyph() { printf "$(printf '\\U%08X' "$1")"; }

# A labelled chart of a codepoint range, 32 glyphs per row with a hex row label.
chart() { # label start-hex end-hex
  local start=$((16#$2)) end=$((16#$3)) n=$((16#$2)) base e
  printf '\n\e[1;36m%s\e[0m  \e[90mU+%04X–%04X\e[0m\n' "$1" "$start" "$end"
  while ((n <= end)); do
    base=$n e=$((n + 31)); ((e > end)) && e=$end
    printf '  \e[90m%05X\e[0m ' "$base"
    while ((n <= e)); do glyph "$n"; ((n++)); done
    printf '\n'
  done
}

# A classic three-segment powerline prompt (fg of each ► is the previous bg).
prompt() {
  local a=$(glyph 0xE0B0)
  printf '\n\e[1;36mpowerline prompt\e[0m\n  '
  printf '\e[48;5;33m\e[38;5;15m ingmar '
  printf '\e[48;5;240m\e[38;5;33m%s\e[38;5;15m ~/src ' "$a"
  printf '\e[48;5;34m\e[38;5;240m%s\e[38;5;15m main ' "$a"
  printf '\e[49m\e[38;5;34m%s\e[0m\n' "$a"
}

clear
printf '\e[1mshellglass glyph gallery\e[0m — rendered from the font, no local install needed\n'
chart  "box drawing"          2500 257F
chart  "block elements"       2580 259F
chart  "legacy sextants"      1FB00 1FB3B
chart  "legacy smooth mosaic" 1FB3C 1FB6F
chart  "legacy eighth blocks" 1FB70 1FB8B
chart  "legacy misc"          1FB8C 1FBAF
chart  "powerline separators" E0B0 E0D4
prompt

printf '\n\e[90mPress Enter to quit.\e[0m'
read -r
