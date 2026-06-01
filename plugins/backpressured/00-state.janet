# Backpressured plugin — shared state + project check discovery.
#
# A dirge port of the "backpressure" loop (lucasfcosta/backpressured):
# drive a goal to completion autonomously while making the MACHINE say
# "no" first — lint, tests, typecheck, and an independent reviewer gate
# every iteration, not a human at the end.
#
# This file holds the mode flag and discovers the project's check
# commands so the discipline prompt can name them concretely.

# Whether the backpressured loop is currently engaged. Set by the
# keyword trigger in on-prompt, or by the /backpressured command.
(var bp-active false)

# True once a file exists (os/stat returns nil / raises for missing).
(defn- bp-exists? [path]
  (truthy? (try (os/stat path) ([_] nil))))

# Discover the project's quality-check commands. A project-root
# BACKPRESSURE.md is authoritative when present (returned verbatim);
# otherwise probe common manifests and suggest the usual commands.
(defn bp-detect-checks []
  (def cwd (harness/get-cwd))
  (defn at [name] (string cwd "/" name))

  (def bp-md (try (slurp (at "BACKPRESSURE.md")) ([_] nil)))
  (if bp-md
    (string "## Project checks — BACKPRESSURE.md (authoritative)\n\n" bp-md)
    (do
      (def lines @[])
      (when (bp-exists? (at "Cargo.toml"))
        (array/push lines
          "Rust: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`"))
      (when (bp-exists? (at "package.json"))
        (array/push lines
          "Node: run the `lint` and `test` scripts in package.json (e.g. `npm run lint`, `npm test`) — read the scripts block for exact names"))
      (when (or (bp-exists? (at "deps.edn")) (bp-exists? (at "project.clj")))
        (array/push lines
          "Clojure: run the test alias (e.g. `clojure -M:test` / `lein test`); lint with clj-kondo if present"))
      (when (bp-exists? (at "pyproject.toml"))
        (array/push lines
          "Python: `pytest`, plus `ruff check` / `mypy` if configured"))
      (when (bp-exists? (at "go.mod"))
        (array/push lines "Go: `go vet ./...`, `go test ./...`"))
      (when (bp-exists? (at "Makefile"))
        (array/push lines
          "Makefile present: prefer `make test` / `make lint` if those targets exist"))
      (if (> (length lines) 0)
        (string "## Project checks (run the ones that apply, every iteration)\n\n- "
                (string/join lines "\n- "))
        (string "## Project checks\n\n"
                "No standard manifest detected — discover the lint/test/build "
                "commands yourself (README, CI config, Makefile) and run them "
                "every iteration.")))))
