#!/usr/bin/env bash
# Regenerates docs/screenshots/*.png by driving the release build under Xvfb with xdotool.
#
#   docker compose -f docker/compose.yml up -d --wait
#   scripts/screenshots.sh                 # every scene
#   scripts/screenshots.sh editing sql     # selected scenes
#
# Needs: Xvfb, xdotool, ImageMagick (import). Scenes never commit changes to the databases.
# Coordinates are relative to the 1360x860 window and assume the default fonts and layout.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$ROOT/docs/screenshots}"
BIN="${BIN:-$ROOT/target/release/tableplus-rs}"
APP_HOME="${APP_HOME:-$(mktemp -d /tmp/tableplus-shots.XXXXXX)}"
export DISPLAY="${DISPLAY_NUM:-:99}"
export LIBGL_ALWAYS_SOFTWARE=1

mkdir -p "$OUT"
if ! xdpyinfo >/dev/null 2>&1; then
  Xvfb "$DISPLAY" -screen 0 1600x1000x24 >/dev/null 2>&1 &
  XVFB_PID=$!
  sleep 1
fi
[[ -x "$BIN" ]] || (cd "$ROOT" && cargo build --release -p tableplus)
"$ROOT/scripts/demo-home.sh" "$APP_HOME" >/dev/null
# A throwaway $HOME so paths in the screenshots read ~/… instead of the build machine's layout.
FAKE_HOME="$APP_HOME/home"
mkdir -p "$FAKE_HOME/.ssh" "$FAKE_HOME/Databases"
install -m 600 "$ROOT/docker/sshd/test_key" "$FAKE_HOME/.ssh/id_ed25519"
cp "$ROOT/testdata/shop.sqlite" "$FAKE_HOME/Databases/shop.sqlite"
sed -i -e 's#"path": "[^"]*shop.sqlite"#"path": "~/Databases/shop.sqlite"#' \
  -e 's#"key_path": "[^"]*"#"key_path": "~/.ssh/id_ed25519"#' "$APP_HOME/connections.json"

cleanup() {
  pkill -x tableplus-rs 2>/dev/null || true
  [[ -n "${XVFB_PID:-}" ]] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

# --- driving helpers ---------------------------------------------------------------------------

OX=0
OY=0
WID=
click() { xdotool mousemove $((OX + $1)) $((OY + $2)) click 1; sleep "${3:-0.4}"; }
dclick() { xdotool mousemove $((OX + $1)) $((OY + $2)) click --repeat 2 --delay 70 1; sleep "${3:-0.6}"; }
hover() { xdotool mousemove $((OX + $1)) $((OY + $2)); sleep "${3:-0.4}"; }
# Drags in ten steps; egui misses a press and release at different points with no motion between.
drag() {
  local i
  xdotool mousemove $((OX + $1)) $((OY + $2)) mousedown 1
  for i in $(seq 1 10); do
    xdotool mousemove $((OX + $1 + ($3 - $1) * i / 10)) $((OY + $2 + ($4 - $2) * i / 10))
    sleep 0.02
  done
  xdotool mouseup 1
  sleep "${5:-0.6}"
}
key() { xdotool key "$@"; sleep 0.35; }
typ() { xdotool type --delay 25 -- "$1"; sleep 0.3; }
park() { xdotool mousemove $((OX + 640)) $((OY + 20)); }
shot() {
  sleep "${2:-0.4}"
  import -window "$WID" "$OUT/$1.png"
  echo "  $OUT/$1.png"
}

# Types multi-line text; newlines are sent as Return (xdotool's own newline is not delivered).
typlines() {
  local first=1 line
  while IFS= read -r line; do
    if [[ $first == 0 ]]; then
      xdotool key Return
      sleep 0.15
    fi
    first=0
    [[ -n "$line" ]] && xdotool type --delay 25 -- "$line"
    sleep 0.2
  done <<<"$1"
  sleep 0.3
}

# Starts the app with a fresh window and the given theme.
launch() {
  pkill -x tableplus-rs 2>/dev/null || true
  sleep 0.5
  printf '{ "theme": "%s" }\n' "${1:-Dark}" >"$APP_HOME/settings.json"
  HOME="$FAKE_HOME" TABLEPLUS_RS_HOME="$APP_HOME" setsid "$BIN" >/tmp/tableplus-shots.log 2>&1 </dev/null &
  disown
  WID="$(xdotool search --sync --onlyvisible --name '^tableplus-rs' | head -1)"
  sleep 1.5
  eval "$(xdotool getwindowgeometry --shell "$WID")"
  OX=$X
  OY=$Y
  xdotool windowfocus --sync "$WID"
}

# Welcome-screen rows are sorted by name.
ROW_MARIADB=237
ROW_SSH=295
ROW_MYSQL=353
ROW_POSTGRES=411
ROW_SQLITE=469
connect() { dclick 700 "$1" 3; }

# Sidebar rows for the Postgres demo database (public schema expanded).
SIDEBAR_PRODUCTS=240
SIDEBAR_USERS=336

# Bottom bar of a table tab.
BAR_ADD=411
BAR_DELETE=445
BAR_Y=842

# --- scenes --------------------------------------------------------------------------------------

scene_welcome() {
  launch Dark
  hover 800 $ROW_POSTGRES 0.5
  shot welcome
}

# Filters users to 12 rows, then stages three edits, a deletion and an insert.
stage_user_edits() {
  connect $ROW_POSTGRES
  click 79 $SIDEBAR_USERS 2
  key ctrl+f
  click 366 89 0.6 # column
  click 314 150 0.5 # id
  click 505 89 0.6 # operator
  click 474 242 0.5 # <=
  click 778 89 0.4
  typ 12
  key Return
  sleep 1.5
  # Rows start at y=180 below the filter bar.
  dclick 600 204
  key ctrl+a
  typ "Alan M. Lovelace"
  key Return
  dclick 950 252
  key ctrl+a
  typ "1250.00"
  key Return
  dclick 740 276
  key ctrl+a
  typ "NZ"
  key Return
  click 300 300 0.4
  click $BAR_DELETE $BAR_Y 0.6
  click $BAR_ADD $BAR_Y 1
  dclick 440 468
  typ "grace@example.com"
  key Return
  dclick 610 468
  typ "Grace Murray"
  key Return
  dclick 740 468
  typ "US"
  key Return
  key Escape
  park
}

scene_editing() {
  launch Dark
  stage_user_edits
  shot table-editing
  click 1221 $BAR_Y 1 # Preview
  park
  shot commit-preview
}

# A two-condition filter and a descending sort on the 2,000,000-row analytics.events table.
scene_large() {
  launch Dark
  connect $ROW_POSTGRES
  click 73 456 1   # analytics schema
  click 60 480 0.6 # its tables
  click 82 504 2.5 # events
  key ctrl+f
  click 366 89 0.6
  click 330 210 0.5 # event_type
  click 505 89 0.6
  click 474 122 0.5 # =
  click 778 89 0.4
  typ purchase
  click 1043 89 0.8 # second condition
  click 366 119 0.6
  click 330 300 0.5 # duration_ms
  click 505 119 0.6
  click 474 242 0.5 # >
  click 778 119 0.4
  typ 4000
  click 320 151 2.5 # Apply
  click 712 185 2   # sort by duration_ms
  click 712 185 2.5 # descending
  park
  shot large-table
}

scene_sql() {
  launch Dark
  connect $ROW_POSTGRES
  key ctrl+t
  sleep 0.8
  typlines "-- Top customers by lifetime value
select u.email, u.country, count(*) as orders, sum(o.total) as revenue
from orders o
join users u on u.id = o.user_id
where o.status in ('paid', 'shipped', 'delivered') and u."
  sleep 1
  park
  shot sql-autocomplete
  typ "is_"
  key Tab
  typlines "
group by u.id
order by revenue desc
limit 100;

select status, count(*) as orders, round(avg(total), 2) as avg_total
from orders group by status order by status;"
  key ctrl+shift+Return
  sleep 3
  park
  shot sql-results
  key ctrl+k
  sleep 0.6
  typ ord
  sleep 0.6
  park
  shot quick-open
}

scene_structure() {
  launch Dark
  connect $ROW_POSTGRES
  click 89 $SIDEBAR_PRODUCTS 2
  click 345 $BAR_Y 2 # Structure
  click 1151 298 0.4
  typ "Unit price in USD"
  click 806 332 0.4
  key ctrl+a
  typ 10
  click 306 437 0.8 # + Column
  click 372 434 0.4
  key ctrl+a
  typ weight_kg
  click 540 434 0.4
  key ctrl+a
  typ "numeric(8,3)"
  click 1151 434 0.4
  typ "Shipping weight"
  click 700 490 0.3
  click 1280 $BAR_Y 1.2 # Review changes
  park
  shot structure-preview
}

# Explain Analyze of a union of two grouped joins on MySQL, as the tree, the graph and the flame chart.
scene_explain() {
  launch Dark
  connect $ROW_MYSQL
  key ctrl+t
  sleep 0.8
  typlines "-- Top customers and categories by delivered revenue
SELECT 'customer' AS kind, u.full_name AS name, COUNT(DISTINCT o.id) AS orders,
  SUM(t.order_total) AS revenue,
  (SELECT MAX(o2.placed_at) FROM orders o2 WHERE o2.user_id = u.id) AS last_order
FROM users u
JOIN orders o ON o.user_id = u.id
JOIN (SELECT order_id, SUM(unit_price * quantity) AS order_total
  FROM order_items GROUP BY order_id) t ON t.order_id = o.id
WHERE o.status = 'delivered'
  AND EXISTS (SELECT 1 FROM order_items oi2
  JOIN products p2 ON p2.id = oi2.product_id
  JOIN categories c2 ON c2.id = p2.category_id
  WHERE oi2.order_id = o.id AND c2.name = 'Books')
GROUP BY u.id, u.full_name
HAVING COUNT(DISTINCT o.id) >= 2
UNION ALL
SELECT 'category', c.name, COUNT(DISTINCT oi.order_id),
  SUM(oi.unit_price * oi.quantity), MAX(o.placed_at)
FROM categories c
JOIN products p ON p.category_id = c.id
JOIN order_items oi ON oi.product_id = p.id
JOIN orders o ON o.id = oi.order_id
WHERE o.status = 'delivered' AND c.name <> 'Books'
  AND NOT EXISTS (SELECT 1 FROM products p3 WHERE p3.category_id = c.id AND p3.stock = 0)
GROUP BY c.id, c.name
ORDER BY revenue DESC, name
LIMIT 20;"
  key Escape
  click 700 150 0.3
  key ctrl+Home
  drag 800 424 800 300 # editor and plan splitter
  key ctrl+shift+e
  sleep 3
  park
  shot explain-tree
  click 337 348 1      # Graph
  drag 800 300 800 150 # give the graph more height
  dclick 504 612 1.5   # fold the customer branch under Filter
  click 998 834 0.8    # zoom in one step
  park
  shot explain-graph
  click 394 236 1      # Flame
  click 400 526 0.8    # the index lookup into the derived table, with its details
  park
  shot explain-flame
}

# Light theme: an SSH-tunnelled connection being tested, then two sessions side by side.
scene_light() {
  launch Light
  key ctrl+n
  sleep 0.8
  click 571 352 0.8 # Tag
  click 548 474 0.5 # staging
  click 610 322 0.3
  typ "Prod replica (staging DB)"
  click 901 322 0.3 # blue
  click 548 554 0.6 # Connect over SSH (the dialog grows and re-centres)
  click 670 326 0.3
  key ctrl+a
  typ postgres
  click 925 326 0.3
  key ctrl+a
  typ 5432
  click 735 356 0.3
  key ctrl+a
  typ tp
  click 660 386 0.3
  typ tp_pass
  click 735 416 0.3
  typ shop
  click 670 524 0.3
  typ 127.0.0.1
  click 925 524 0.3
  key ctrl+a
  typ 22229
  click 735 554 0.3
  typ tunnel
  click 624 584 0.5 # Private key (grows again)
  click 692 599 0.3
  typ "~/.ssh/id_ed25519"
  click 421 717 4   # Test
  park
  shot connection-dialog
  click 786 717 0.6 # Cancel
  connect $ROW_MYSQL
  click 89 264 2   # products
  click 82 240 2.5 # orders
  click 600 232 0.4
  click 18 19 1 # home
  connect $ROW_POSTGRES
  click 79 $SIDEBAR_USERS 2.5
  click 100 19 1 # back to the MySQL session
  park
  shot light-mysql
}

SCENES=(welcome editing large sql structure explain light)
if [[ $# -gt 0 ]]; then SCENES=("$@"); fi
for s in "${SCENES[@]}"; do
  echo "scene: $s"
  "scene_$s"
done
