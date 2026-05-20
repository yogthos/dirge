# Custom-provider registration example (P1)
#
# Demonstrates the P1 plugin-providers API:
#   (harness/register-provider name type base-url &opt api-key-env)
#
# At startup the host harvests every registered provider and installs
# them into the runtime provider resolver — alongside any providers
# declared in config.json. Use `/model <model-id>` to select a model
# served by the registered provider; the same name is what you'd
# pass to `--provider` on the command line.
#
# Plugins re-register on every startup (no persistence — same model
# as pi). Config-declared `custom_providers` always win on name
# collision, so user intent trumps plugin defaults.

(def hooks [])

# Register a few common local LLM endpoints. Real plugins might
# probe the endpoint with HTTP first, but we keep it simple here.

(harness/register-provider
  "local-vllm"
  "openai"
  "http://localhost:8000/v1"
  "LOCAL_VLLM_API_KEY")

(harness/register-provider
  "local-ollama-openai"
  "openai"
  "http://localhost:11434/v1"
  "OLLAMA_API_KEY")

(harness/register-provider
  "lmstudio"
  "openai"
  "http://localhost:1234/v1")
# No api-key-env arg → host falls back to the default for the type
# (CUSTOM_API_KEY for plain "openai"-type providers without override).
