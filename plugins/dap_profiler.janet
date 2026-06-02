# dap-profiler — sampling profiler driven by DAP Janet FFI
#
# Uses timer-based sampling: hooks on-tool-end and periodically fires
# `dap/stack-trace` against the active session, aggregating callers
# by function name. After N samples it prints a top-N hotspot report.
#
# Architecture:
#   `/dap-profile start <interval-ms>` arms the profiler.
#   Each tick calls `dap/stack-trace` (Janet FFI -> DapSessionManager
#   -> DAP stackTrace request -> adapter -> real stack frames).
#   Frames are aggregated by function name. Over many samples,
#   frequently-called functions dominate the report.
#   `/dap-profile stop` prints the report and resets.
#   `/dap-profile report` prints current report without stopping.
#
# This is a STATISTICAL profiler — same algorithm as `perf record`.

(def hooks ["on-tool-end"])

# ── State ────────────────────────────────────────────────────────────

(var profiling false)
(var profile-next-sample 0)  # epoch ms of next sample
(var profile-interval 200)   # ms between samples
(var profile-max-samples 100)
(var profile-samples 0)
(var profile-counts @{})     # frame-name -> count

# ── Hook — sample on each tool-end if profiling ──────────────────────

# Monotonic-ish millisecond clock. `os/clock` returns seconds as a double;
# `os/time` is whole seconds, which made the `profile-interval` (ms) math
# off by ~1000x (a "200ms" profile sampled every 200s).
(defn- now-ms [] (* (os/clock) 1000))

(defn on-tool-end [ctx]
  (when (and profiling (>= (now-ms) profile-next-sample))
    (set profile-next-sample (+ (now-ms) profile-interval))
    (take-sample)))

# ── Sample logic ─────────────────────────────────────────────────────

(defn- take-sample []
  (def frames-str (dap/stack-trace))
  (when frames-str
    # Janet has no JSON parser, so match function names with regex.
    # The stack-trace JSON has '"name": "function_name"' patterns.
    (var idx 0)
    (var found 0)
    (while (and (< found 10)
                (>= idx 0)
                (< idx (length frames-str)))
      (def name-start (string/find "\"name\": \"" frames-str idx))
      (if (not name-start)
        (break))
      (set name-start (+ name-start 9))
      (def name-end (string/find "\"" frames-str name-start))
      (when name-end
        (def name (string/slice frames-str name-start name-end))
        # Skip runtime/launcher frames
        (when (and (not (string/find "_run_" name))
                   (not (string/find "runpy" name))
                   (not= name "<module>")
                   (not= name "_run_code")
                   (not= name "_run_module_as_main"))
          (def count (get profile-counts name 0))
          (put profile-counts name (+ count 1))
          (set found (+ found 1)))
        (set idx (+ name-end 1)))))
  (set profile-samples (+ profile-samples 1))
  (when (>= profile-samples profile-max-samples)
    (harness/notify (profiler-report) :info)
    (set profiling false)))

# ── Report generator ────────────────────────────────────────────────

(defn- profiler-report []
  (def entries @[])
  (loop [[k v] :pairs profile-counts]
    (array/push entries [v k]))
  (sort entries (fn [a b] (> (get a 0) (get b 0))))
  (var out "PROFILER REPORT\n")
  (set out (string out "Samples: " profile-samples
                   "  Interval: " profile-interval "ms\n\n"))
  (var rank 0)
  (loop [entry :in entries]
    (when (< rank 20)
      (def count (get entry 0))
      (def key (get entry 1))
      (def pct (math/round (* 100 (/ count profile-samples))))
      (set out (string out (string "  " rank ". " pct "%  " key "\n")))
      (set rank (+ rank 1))))
  out)

# ── Slash commands ──────────────────────────────────────────────────

(defn profile-start [args]
  (when (not (dap/session-active?))
    (break "No active DAP session — launch a program first"))
  (def interval (if (empty? args) 200 (math/parse-int args)))
  (set profiling true)
  (set profile-interval (max 50 interval))
  (set profile-max-samples 100)
  (set profile-samples 0)
  (set profile-next-sample (+ (now-ms) profile-interval))
  (set profile-counts @{})
  (string "Profiling started — " profile-max-samples
          " samples at " profile-interval "ms intervals"))

(defn profile-stop [_args]
  (when (not profiling) (break "Profiler not running"))
  (set profiling false)
  (profiler-report))

(defn profile-report [_args]
  (if profiling (profiler-report)
    "Profiler not running — start with /dap-profile"))

(defn profile-clear [_args]
  (set profile-counts @{})
  (set profile-samples 0)
  "Profile data cleared")

(harness/register-command "dap-profile" "profile-start")
(harness/register-command "dap-profile-stop" "profile-stop")
(harness/register-command "dap-profile-report" "profile-report")
(harness/register-command "dap-profile-clear" "profile-clear")
