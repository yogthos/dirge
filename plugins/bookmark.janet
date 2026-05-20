# Bookmark example
#
# Demonstrates the P2 plugin-entry + renderer APIs:
#   (harness/append-entry "type" "data" &opt display)
#     records a typed entry on the session timeline. Survives
#     save/load. The host's default renderer dumps the raw data
#     dim; a registered renderer formats it however the plugin
#     wants.
#   (harness/register-renderer "type" "fn-name")
#     associates a custom_type with a Janet function the host
#     calls to render entries of that type.
#   (harness/render color text)
#     called from inside a registered renderer; emits one line
#     of output with the given color name (cyan, red, etc.).
#
# Usage:
#   /bookmark <label>  — record a bookmark on the timeline.
#   /bookmarks         — list all recorded bookmarks for this session.

(def hooks [])

(defn render-bookmark [data]
  # Data is the bookmark label string (we chose plain text;
  # plugins can equally store JSON if they want richer payloads).
  (harness/render "cyan" (string "★ " data)))

(harness/register-renderer "bookmark" "render-bookmark")

(defn bookmark-handler [args]
  (let [label (if (= (length args) 0) "(unlabeled)" args)]
    (harness/append-entry "bookmark" label)
    (string "bookmarked: " label)))

(harness/register-command "bookmark" "bookmark-handler")
