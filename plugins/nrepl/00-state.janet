# nREPL client state, bencode codec, and protocol functions.
#
# nREPL is a TCP-based protocol using bencode-encoded messages.
# Each message is a bencode dictionary with at minimum an "op" key.
# This file provides encode/decode + connect/disconnect/eval.

# ── byte constants ──────────────────────────────────────────────
(def i-byte 105)  # 'i'
(def l-byte 108)  # 'l'
(def d-byte 100)  # 'd'
(def e-byte 101)  # 'e'
(def dash-byte 45)   # '-'
(def zero-byte 48)   # '0'
(def nine-byte 57)   # '9'
(def colon-byte 58)  # ':'

# ── bencode decoder ─────────────────────────────────────────────

(defn- parse-int [s pos]
  (var p pos)
  (var neg false)
  (if (= (get s p) dash-byte)
    (do (set neg true) (set p (+ p 1))))
  (var num 0)
  (while (not= (get s p) e-byte)
    (set num (+ (* num 10) (- (get s p) zero-byte)))
    (set p (+ p 1)))
  [(if neg (- 0 num) num) (+ p 1)])

(defn- parse-str [s pos]
  (var p pos)
  (var len 0)
  (while (not= (get s p) colon-byte)
    (set len (+ (* len 10) (- (get s p) zero-byte)))
    (set p (+ p 1)))
  (set p (+ p 1))  # skip ':'
  (def val (string/slice s p (+ p len)))
  [val (+ p len)])

# Forward declaration — set below after parse-list/parse-dict are defined.
(var parse-value nil)

(defn- parse-list [s pos]
  (var p pos)
  (var items @[])
  (while (not= (get s p) e-byte)
    (def [v np] (parse-value s p))
    (array/push items v)
    (set p np))
  [items (+ p 1)])

(defn- parse-dict [s pos]
  (var p pos)
  (var tbl @{})
  (while (not= (get s p) e-byte)
    (def [k kp] (parse-str s p))
    (def [v vp] (parse-value s kp))
    (put tbl k v)
    (set p vp))
  [tbl (+ p 1)])

(set parse-value
  (fn [s pos]
    (def b (get s pos))
    (cond
      (= b i-byte) (parse-int s (+ pos 1))
      (= b l-byte) (parse-list s (+ pos 1))
      (= b d-byte) (parse-dict s (+ pos 1))
      (and (>= b zero-byte) (<= b nine-byte)) (parse-str s pos)
      (error (string "unexpected bencode byte " b " at " pos)))))

(defn bencode-decode [s]
  "Parse one bencode-encoded value from string s."
  (def [v _] (parse-value s 0))
  v)

# ── bencode encoder ─────────────────────────────────────────────

(defn- encode-any [v]
  (cond
    (number? v) (string "i" (string/format "%d" v) "e")
    (string? v) (string (length v) ":" v)
    (buffer? v) (string (length v) ":" v)
    (indexed? v)
    (string "l" (string/join (map encode-any v)) "e")
    (dictionary? v)
    (do
      (def ks (sorted (keys v)))
      (string "d"
              (string/join (map (fn [k] (string (encode-any (string k))
                                                (encode-any (get v k))))
                                ks))
              "e"))
    (error (string "cannot bencode: " (type v)))))

(defn bencode-encode [v]
  "Encode a Janet value as a bencode string."
  (encode-any v))

# ── nREPL state ─────────────────────────────────────────────────

(var nrepl-conn nil)      # TCP socket
(var nrepl-session nil)   # nREPL session id string
(var nrepl-host "127.0.0.1")
(var nrepl-port nil)
(var nrepl-connected false)
(var nrepl-eval-timeout 120)  # per-eval timeout in seconds
(var nrepl-current-eval-id nil)  # active eval id for interrupt
(var nrepl-rbuf @"")      # bytes read but not yet decoded (see below)

# ── nREPL protocol ──────────────────────────────────────────────

(defn- try-decode-buffered [b]
  "Attempt to parse ONE complete bencode value from buffer `b`.
  Returns [value consumed-bytes], or nil when `b` holds only a
  partial (incomplete) message. Incomplete data makes the byte-level
  parsers run off the end and raise, which we treat as 'need more'."
  (if (= (length b) 0)
    nil
    (try
      (parse-value (string b) 0)
      ([_] nil))))

(defn nrepl-read-msg [conn &opt timeout-secs]
  "Read one complete bencode-encoded nREPL message, buffering across
  socket reads. A single TCP read can return a partial message OR
  several coalesced messages; `nrepl-rbuf` retains undecoded bytes so
  neither case loses data. Returns a parsed dict. Raises on timeout
  or disconnect. timeout-secs defaults to nil (blocking)."
  (var result nil)
  (while (nil? result)
    (if-let [decoded (try-decode-buffered nrepl-rbuf)]
      (do
        (def [v consumed] decoded)
        # Keep any bytes past this message for the next call so a
        # coalesced follow-up message isn't dropped.
        (set nrepl-rbuf (buffer (string/slice nrepl-rbuf consumed)))
        (set result v))
      (do
        (def buf (if timeout-secs
                   (net/read conn 65536 nil timeout-secs)
                   (net/read conn 65536)))
        (if (or (nil? buf) (= (length buf) 0))
          (error "nREPL connection closed by server"))
        (buffer/push-string nrepl-rbuf buf))))
  result)

(defn nrepl-send-msg [conn msg]
  "Send a single nREPL message (as a Janet dict/table)."
  (net/write conn (bencode-encode msg)))

(defn- connect-nrepl-inner [host port]
  (def conn (net/connect host port :stream))
  # Fresh socket → drop any leftover bytes from a previous session.
  (set nrepl-rbuf @"")
  (nrepl-send-msg conn @{"op" "clone" "id" "dirge-clone"})
  (def clone-resp (nrepl-read-msg conn))
  (def session (get clone-resp "new-session"))
  (set nrepl-conn conn)
  (set nrepl-session session)
  (set nrepl-host host)
  (set nrepl-port port)
  (set nrepl-connected true)
  session)

(defn nrepl-connect
  "Connect to an nREPL server at host:port. Creates a new session
  via clone. Returns a status string."
  [host port]
  (if nrepl-connected
    (do
      (try (:close nrepl-conn) ([_] nil))
      (set nrepl-connected false)))
  (def session (connect-nrepl-inner host port))
  (string "connected to nREPL at " host ":" port " — session: " session))

(defn nrepl-disconnect []
  "Close the nREPL session and TCP connection."
  (if (not nrepl-connected)
    "not connected"
    (do
      (try
        (do
          (nrepl-send-msg nrepl-conn
                          @{"op" "close" "session" nrepl-session})
          (:close nrepl-conn))
        ([err] nil))
      (set nrepl-conn nil)
      (set nrepl-session nil)
      (set nrepl-connected false)
      (set nrepl-rbuf @"")
      "disconnected from nREPL")))

# ── paren repair ─────────────────────────────────────────────────
#
# LLMs frequently emit Clojure code with unbalanced delimiters.
# Walks code skipping strings and comments, tracking open
# delimiters on a stack, and appends any missing closers at the
# end so nREPL gets valid syntax on the first attempt.
#
# All delimiter matching uses byte values (integers), keepping
# the hot path free of string conversions.

(def- open-paren  40)  # (
(def- close-paren 41)  # )
(def- open-brack  91)  # [
(def- close-brack 93)  # ]
(def- open-brace  123) # {
(def- close-brace 125) # }
(def- semicolon   59)  # ;
(def- doublequote 34)  # "
(def- backslash   92)  # \
(def- newline     10)  # \n

(def- closer-for @{open-paren close-paren
                   open-brack close-brack
                   open-brace close-brace})

(defn paren-repair [code]
  (var stack @[])
  (var i 0)
  (var in-str false)
  (var len (length code))
  (while (< i len)
    (def ch (get code i))
    (cond
      # Line comment → skip to newline
      (= ch semicolon)
      (do
        (while (and (< i len) (not= (get code i) newline))
          (set i (+ i 1)))
        (set i (+ i 1)))
      # Unescaped quote → toggle string mode
      (= ch doublequote)
      (do
        (if (and (> i 0) (= (get code (- i 1)) backslash))
          nil
          (set in-str (not in-str)))
        (set i (+ i 1)))
      # Inside string → skip
      in-str
      (set i (+ i 1))
      # Opening delimiter
      (or (= ch open-paren) (= ch open-brack) (= ch open-brace))
      (do
        (array/push stack ch)
        (set i (+ i 1)))
      # Closing delimiter — pop if matches top of stack
      (or (= ch close-paren) (= ch close-brack) (= ch close-brace))
      (do
        (if (and (> (length stack) 0)
                 (= (get closer-for (last stack)) ch))
          (array/pop stack)
          nil)  # extra closer, ignore
        (set i (+ i 1)))
      # Regular character
      (set i (+ i 1))))
  (if (= (length stack) 0)
    code
    (string code
            (string/join (map (fn [b] (string/from-bytes (get closer-for b)))
                              (reverse stack))))))

(defn nrepl-interrupt []
  "Send an interrupt op for the currently in-flight eval (if any)."
  (when (and nrepl-connected nrepl-current-eval-id)
    (try
      (nrepl-send-msg nrepl-conn
                      @{"op" "interrupt"
                        "interrupt-id" nrepl-current-eval-id
                        "session" nrepl-session})
      ([_] nil))))

(defn- nrepl-eval-inner [code]
  (def eval-id (string "dirge-eval-" (os/time)))
  (set nrepl-current-eval-id eval-id)
  (nrepl-send-msg nrepl-conn
                  @{"op" "eval"
                    "code" code
                    "id" eval-id
                    "session" nrepl-session})
  (var values @[])
  (var out "")
  (var err "")
  (var done false)
  (var ns "")
  # os/time is in SECONDS (Unix epoch), so elapsed is a plain
  # difference — no /1000. (The previous code divided by 1000, which
  # made the timeout ~1000x too long and the interrupt never fire.)
  (var start-s (os/time))
  (while (not done)
    (def elapsed-s (- (os/time) start-s))
    (if (>= elapsed-s nrepl-eval-timeout)
      (do
        (nrepl-interrupt)
        (set nrepl-current-eval-id nil)
        (error (string "nREPL eval timed out after " nrepl-eval-timeout "s"))))
    (def remaining (- nrepl-eval-timeout elapsed-s))
    (def read-timeout (max 2 remaining))  # at least 2s per read
    (def resp (nrepl-read-msg nrepl-conn read-timeout))
    (if-let [o (get resp "out")] (set out (string out o)))
    (if-let [e (get resp "err")] (set err (string err e)))
    (if-let [v (get resp "value")] (array/push values v))
    (if-let [n (get resp "ns")] (set ns n))
    (if-let [statuses (get resp "status")]
      (when (indexed? statuses)
        (each s statuses
          (if (= s "done") (set done true))))))
  (set nrepl-current-eval-id nil)
  @{"result" (string/join values "\n")
    "out" out
    "err" err
    "ns" ns})

(defn nrepl-eval
  "Evaluate Clojure code on the connected nREPL server.
  Automatically repairs unbalanced delimiters before sending.
  Returns a dict with keys: result, out, err, ns."
  [code]
  (if (not nrepl-connected)
    (error "not connected to nREPL — use /nrepl-connect first"))
  (def repaired (paren-repair code))
  (def result (nrepl-eval-inner repaired))
  (def result-table @{:result (get result "result")
                       :out (get result "out")
                       :err (get result "err")
                       :ns (get result "ns")})
  (if (not= repaired code)
    (put result-table :repaired repaired))
  result-table)

(defn nrepl-status []
  "Return a human-readable connection status string."
  (if nrepl-connected
    (string "connected to " nrepl-host ":" nrepl-port
            " — session: " nrepl-session)
    "not connected"))

# ── utility ────────────────────────────────────────────────────

(defn scan-number [s]
  "Parse a number from string s. Returns number or nil."
  (def s-trim (string/trim s))
  (if (= s-trim "") nil
    (do
      (var n 0)
      (var ok true)
      (each ch s-trim
        (if (and (>= ch 48) (<= ch 57))
          (set n (+ (* n 10) (- ch 48)))
          (set ok false)))
      (if ok n nil))))

# ── minimal JSON value extractor ────────────────────────────────

(defn- json-extract-string [s key]
  "Extract a string value for `key` from a flat JSON object string.
  Returns nil if the key is not found."
  (def search (string "\"" key "\""))
  (if-let [start (string/find search s)]
    (let [after-key (string/slice s (+ start (length search)))
          colon (string/find ":" after-key)]
      (if colon
        (let [after-colon (string/trim (string/slice after-key (+ colon 1)))
              b (get after-colon 0)]
          (if (= b 34)  # '"'
            (let [escaped-rest (string/slice after-colon 1)
                  end-quote (string/find "\"" escaped-rest)]
              (if end-quote
                (string/slice escaped-rest 0 end-quote)
                nil))
            nil))
        nil))
    nil))
