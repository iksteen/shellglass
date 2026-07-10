#!/usr/bin/env bash
# A rendering showcase for shellglass. Run it under the viewer:
#
#     shellglass serve -- ./examples/showcase.sh
#
# then open the printed URL. It composes one screen of terminal art — line
# weights, corner styles, thin↔thick intersections, blocks/shades/mosaics, the
# modern SGR text styles (undercurl & friends) and a powerline prompt — all
# rendered from the font with no local install. The renderer draws box-drawing
# as crisp device-pixel geometry, so mixed-weight junctions (┿ ╂ ┝) and tiling
# stay sharp. Wants ≥30 rows. Press Enter to quit.
set -u
export LC_ALL=${LC_ALL:-C.UTF-8}

RST=$'\e[0m'
at()   { printf '\e[%d;%dH' "$1" "$2"; }          # move cursor to row,col
sgr()  { printf '\e[%sm' "$1"; }                  # set graphic rendition
rep()  { local n=$1 s=$2 o=; while ((n-- > 0)); do o+=$s; done; printf '%s' "$o"; }

# A multi-line art block, positioned. Reads lines from stdin (a heredoc).
#   art ROW COL [COLOR] <<'EOF' … EOF
art() {
  local r=$1 col=$2 clr=${3:-} line
  [ -n "$clr" ] && sgr "$clr"
  while IFS= read -r line; do at "$r" "$col"; printf '%s' "$line"; ((r++)); done
  printf '%s' "$RST"
}

# A simple box. box ROW COL W H STYLE [COLOR] [LABEL]
box() {
  local r=$1 col=$2 w=$3 h=$4 s=$5 clr=${6:-} lbl=${7:-} tl tr bl br hz vt i
  case $s in
    light)  tl=┌ tr=┐ bl=└ br=┘ hz=─ vt=│ ;;
    heavy)  tl=┏ tr=┓ bl=┗ br=┛ hz=━ vt=┃ ;;
    double) tl=╔ tr=╗ bl=╚ br=╝ hz=═ vt=║ ;;
    round)  tl=╭ tr=╮ bl=╰ br=╯ hz=─ vt=│ ;;
    dash)   tl=┏ tr=┓ bl=┗ br=┛ hz=┅ vt=┇ ;;
  esac
  [ -n "$clr" ] && sgr "$clr"
  at "$r" "$col";       printf '%s%s%s' "$tl" "$(rep $((w - 2)) "$hz")" "$tr"
  for ((i = 1; i < h - 1; i++)); do at $((r + i)) "$col"; printf '%s%*s%s' "$vt" $((w - 2)) '' "$vt"; done
  at $((r + h - 1)) "$col"; printf '%s%s%s' "$bl" "$(rep $((w - 2)) "$hz")" "$br"
  printf '%s' "$RST"
  [ -n "$lbl" ] && { at $((r + h / 2)) $((col + (w - ${#lbl}) / 2)); printf '%s' "$lbl"; }
}

# A plus sign in a chosen weight combo. plus ROW COL VERT HORIZ CROSS
plus() {
  local r=$1 col=$2 v=$3 h=$4 x=$5
  at "$r" $((col + 1)); printf '%s' "$v"
  at $((r + 1)) "$col"; printf '%s%s%s' "$h" "$x" "$h"
  at $((r + 2)) $((col + 1)); printf '%s' "$v"
}

caption() { at "$1" "$2"; sgr '1;94'; printf '▸ %s' "$3"; printf '%s' "$RST"; }

# ── compose ───────────────────────────────────────────────────────────────────
printf '\e[?25l'                                  # hide cursor
trap 'printf "\e[?25h%s\n" "$RST"' EXIT
clear

sgr '1;96'; at 1 24; printf 'shellglass · box-drawing showcase'; printf '%s' "$RST"

# Corner & weight styles.
caption 3 2 'line weights & corner styles'
box 4  2  15 5 light  '96' 'light'
box 4 18  15 5 heavy  '93' 'heavy'
box 4 34  15 5 double '92' 'double'
box 4 50  15 5 round  '95' 'rounded'
box 4 66  14 5 dash   '91' 'dashed'

# Thin ↔ thick intersections.
caption 10 2 'thin ↔ thick intersections'

# Heavy frame, light interior grid — junctions mix weight (┯ ┠ ┨ ┷ ┼).
art 11 2 '96' <<'EOF'
┏━━━━━━┯━━━━━┓
┃ mode │ fps ┃
┠──────┼─────┨
┃ push │  30 ┃
┃ hub  │  ∞  ┃
┗━━━━━━┷━━━━━┛
EOF

# A heavy beam pierces a light box (┰ ┝ ╋ ┥ ┸).
art 11 20 '92' <<'EOF'
┌────┰────┐
│    ┃    │
┝━━━━╋━━━━┥
│    ┃    │
└────┸────┘
EOF

# Every weighted cross, U+253C–254B.
at 11 34; sgr '2;37'; printf 'weighted crosses'; printf '%s' "$RST"
art 12 34 '93' <<'EOF'
┼ ┽ ┾ ┿
╀ ╁ ╂ ╃
╄ ╅ ╆ ╇
╈ ╉ ╊ ╋
EOF

# The four fundamental weight combos as plus signs.
sgr '95'
plus 12 52 '│' '─' '┼'   # all light
plus 12 60 '┃' '━' '╋'   # all heavy
plus 12 68 '│' '━' '┿'   # thin vert, thick horiz
plus 12 76 '┃' '─' '╂'   # thick vert, thin horiz
printf '%s' "$RST"
at 15 52; sgr '2;37'; printf ' ┼   ╋   ┿   ╂'; printf '%s' "$RST"

# Blocks, shades, mosaics.
caption 18 2 'blocks · shades · mosaics'
at 19 2;  sgr '37'; printf 'shades  '; sgr '97'
printf '%s%s%s%s' "$(rep 4 ' ')" "$(rep 4 '░')" "$(rep 4 '▒')"; printf '%s%s' "$(rep 4 '▓')" "$(rep 4 '█')"
printf '%s' "$RST"
at 19 44; sgr '37'; printf 'eighths '; sgr '96'; printf '▏▎▍▌▋▊▉█'; printf '%s' "$RST"

at 20 2;  sgr '37'; printf 'blocks  '; printf '%s' "$RST"
grn=(22 28 34 40 70 76 82 46); i=0
for ch in ▁ ▂ ▃ ▄ ▅ ▆ ▇ █; do sgr "38;5;${grn[i]}"; printf '%s' "$ch"; ((i++)); done; printf '%s' "$RST"
at 20 24; sgr '37'; printf 'spark '; sgr '93'; printf '▁▂▄▆█▆▄▂▁▃▅▇█▅▂▁▂▅█▅▂▁'; printf '%s' "$RST"

at 21 2;  sgr '37'; printf 'sextant '; sgr '95'; printf '🬀🬃🬦🬭🬹🬞🬂🬰🬔🬧🬋🬻🬕🬬🬏🬭🬤🬐🬺🬖'; printf '%s' "$RST"
at 21 44; sgr '37'; printf 'quads '; sgr '92'; printf '▖▗▘▙▚▛▜▝▞▟'; printf '%s' "$RST"

# Text styles: classic attributes, the five underline styles (SGR 4:n),
# strikethrough, and colored underlines (SGR 58) — editor-diagnostic style.
caption 23 2 'text styles'
styled() { at "$1" "$2"; sgr "$3"; printf '%s' "$4"; printf '%s' "$RST"; }

styled 24 2  '1'     'bold'
styled 24 9  '2'     'dim'
styled 24 15 '3'     'italic'
styled 24 24 '7'     'inverse'
styled 24 34 '9'     'strike'
styled 24 43 '1;3;9' 'bold italic strike'

styled 25 2  '4'   'single'
styled 25 11 '4:2' 'double'
styled 25 20 '4:3' 'curly'
styled 25 28 '4:4' 'dotted'
styled 25 37 '4:5' 'dashed'
styled 25 46 '21'  'double via SGR 21'

# What helix/neovim diagnostics emit: an underline with its own color (58),
# independent of the text color.
styled 26 2  '4:3;58;5;196'     'spellling'
styled 26 13 '4:3;58;5;220'     'deprecated()'
styled 26 27 '4:4;58;5;39'      'hint'
styled 26 33 '4;58;2;0;200;120' 'truecolor line'
styled 26 49 '9;4:3;58;5;196'   'strike + red curl'

# A powerline prompt (each ► inherits the previous segment's colour).
a=$(printf '\ue0b0')
at 28 2
sgr '48;5;33'; sgr '38;5;15'; printf ' ingmar '
sgr '48;5;240'; sgr '38;5;33'; printf '%s' "$a"; sgr '38;5;15'; printf ' ~/src '
sgr '48;5;34'; sgr '38;5;240'; printf '%s' "$a"; sgr '38;5;15'; printf ' main '
sgr '49'; sgr '38;5;34'; printf '%s' "$a"; printf '%s' "$RST"

at 29 2; sgr '2;37'; printf 'Press Enter to quit.'; printf '%s' "$RST"
read -r
