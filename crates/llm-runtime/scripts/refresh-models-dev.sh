#!/usr/bin/env bash
set -euo pipefail

# Refresh only the provider slices shipped by LingXi. OpenRouter is sourced
# from its official Models API instead of models.dev so model ids, prices,
# limits, modalities, and free variants match the route users actually call.
# The ChatGPT-login slice is hand-authored because that backend exposes its
# catalog dynamically.
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$(cd "$script_dir/.." && pwd)"
data_dir="$crate_dir/data/models-dev"
source_url="${MODELS_DEV_URL:-https://models.dev/api.json}"
openrouter_source_url="${OPENROUTER_MODELS_URL:-https://openrouter.ai/api/v1/models?output_modalities=text}"
models_dev_snapshot="$(mktemp)"
openrouter_snapshot="$(mktemp)"
trap 'rm -f "$models_dev_snapshot" "$openrouter_snapshot"' EXIT

curl --fail --silent --show-error --location "$source_url" --output "$models_dev_snapshot"
curl --fail --silent --show-error --location "$openrouter_source_url" --output "$openrouter_snapshot"

# OpenRouter publishes per-token prices while LingXi's catalog stores
# per-million-token prices. Preserve only facts present in the official
# response; absent prices and limits stay absent rather than being guessed.
jq --arg source "$openrouter_source_url" '
  def per_million($value):
    try (($value | tonumber) * 1000000) catch null;
  def supports($parameter):
    ((.supported_parameters // []) | index($parameter)) != null;
  def without_nulls:
    with_entries(select(.value != null));

  {
    api: "https://openrouter.ai/api/v1",
    source: $source,
    name: "OpenRouter",
    env: ["OPENROUTER_API_KEY"],
    id: "openrouter",
    models: (
      .data
      | map({
          key: .id,
          value: (
            {
              id: .id,
              name: .name,
              description: .description,
              release_date: (
                if (.created | type) == "number"
                then (.created | gmtime | strftime("%Y-%m-%d"))
                else null
                end
              ),
              attachment: (
                (.architecture.input_modalities // [])
                | any(. != "text")
              ),
              temperature: supports("temperature"),
              tool_call: supports("tools"),
              reasoning: (
                (.reasoning != null)
                or supports("reasoning")
                or supports("include_reasoning")
                or supports("reasoning_effort")
              ),
              reasoning_options: ([
                if ((.reasoning.supported_efforts? // []) | length) > 0
                then {
                  type: "effort",
                  values: (
                    .reasoning.supported_efforts
                    | sort_by(
                        . as $effort
                        | ({
                            none: 0,
                            minimal: 1,
                            low: 2,
                            medium: 3,
                            high: 4,
                            xhigh: 5,
                            max: 6
                          }[$effort] // 99)
                      )
                  )
                }
                else empty
                end,
                if (
                  (.reasoning != null)
                  and ((.reasoning.mandatory // false) | not)
                  and supports("reasoning")
                )
                then {type: "toggle"}
                else empty
                end
              ]),
              structured_output: supports("structured_outputs"),
              modalities: {
                input: (.architecture.input_modalities // ["text"]),
                output: (.architecture.output_modalities // ["text"])
              },
              cost: (
                if (
                  per_million(.pricing.prompt) != null
                  and per_million(.pricing.completion) != null
                )
                then ({
                  input: per_million(.pricing.prompt),
                  output: per_million(.pricing.completion),
                  cache_read: per_million(.pricing.input_cache_read),
                  cache_write: per_million(.pricing.input_cache_write),
                  reasoning: per_million(.pricing.internal_reasoning)
                } | without_nulls)
                else null
                end
              ),
              limit: (
                if (
                  (.context_length // 0) > 0
                  and (.top_provider.max_completion_tokens // 0) > 0
                )
                then {
                  context: .context_length,
                  output: .top_provider.max_completion_tokens
                }
                else null
                end
              )
            }
            | without_nulls
          )
        })
      | from_entries
    )
  }
' "$openrouter_snapshot" > "$data_dir/openrouter.json"

while IFS=$'\t' read -r provider_id output_name; do
  jq --arg id "$provider_id" -e '.[$id]' "$models_dev_snapshot" > "$data_dir/$output_name.json"
done <<'EOF'
moonshotai-cn	kimi
kimi-for-coding	kimi-code
zhipuai-coding-plan	zhipuai-coding-plan
zai	zai
openai	openai
github-copilot	github-copilot
google	gemini
EOF

# DeepSeek is vendored through a filter rather than dumped verbatim. Upstream
# still lists the retired `deepseek-v4-flash` / `deepseek-v4-flash-vision-exp`
# ids (api.deepseek.com only advertises `deepseek-flash` and `deepseek-v4-pro`,
# and routes the old names to Flash), and it mirrors DeepSeek's OFF-PEAK column,
# which is exactly half the list price -- vendoring that verbatim would halve
# every cost estimate during peak hours. Keep the two live ids and restate the
# peak list prices from https://api-docs.deepseek.com/quick_start/pricing.
jq -e '
  .deepseek
  | .models |= {
      "deepseek-flash": (
        .["deepseek-flash"]
        + { cost: { input: 0.3, output: 1.2, reasoning: 1.2, cache_read: 0.006 } }
      ),
      "deepseek-v4-pro": (
        .["deepseek-v4-pro"]
        + { cost: { input: 1.32, output: 3.96, reasoning: 3.96, cache_read: 0.044 } }
      )
    }
' "$models_dev_snapshot" > "$data_dir/deepseek.json"

for file in "$data_dir"/*.json; do
  jq -e '.id and .name and (.models | type == "object")' "$file" >/dev/null
done

# Keep the refresh reproducible and fail loudly when upstream removes, renames,
# or deprecates a model that LingXi exposes in its curated pickers. Anthropic is
# intentionally absent: its first-party table is maintained in Rust, while the
# ChatGPT-account slice is hand-authored and validated below.
while IFS=$'\t' read -r slice_name model_id; do
  jq --arg model "$model_id" -e '
    .models[$model]
    and ((.models[$model].status // "") != "deprecated")
  ' "$data_dir/$slice_name.json" >/dev/null || {
    echo "Curated model missing or deprecated: $slice_name/$model_id" >&2
    exit 1
  }
done <<'EOF'
openrouter	openrouter/auto
openrouter	openrouter/free
openrouter	~anthropic/claude-fable-latest
openrouter	~anthropic/claude-opus-latest
openrouter	~anthropic/claude-sonnet-latest
openrouter	~openai/gpt-latest
openrouter	~openai/gpt-mini-latest
openrouter	~google/gemini-pro-latest
openrouter	~google/gemini-flash-latest
openrouter	~deepseek/deepseek-v4-flash-latest
openrouter	~moonshotai/kimi-latest
openrouter	~z-ai/glm-latest
openrouter	~z-ai/glm-flash-latest
openrouter	~x-ai/grok-latest
openrouter	openai/gpt-chat-latest
openrouter	cohere/north-mini-code:free
openrouter	z-ai/glm-5.2:free
openrouter	thinkingmachines/inkling:free
openrouter	thinkingmachines/inkling-small:free
openrouter	minimax/minimax-m3:free
openrouter	minimax/minimax-m2.7:free
openrouter	poolside/laguna-s-2.1:free
openrouter	inclusionai/ling-3.0-flash-fin:free
openrouter	nvidia/nemotron-3.5-lightning:free
openrouter	nvidia/nemotron-3-ultra-550b-a55b:free
deepseek	deepseek-flash
deepseek	deepseek-v4-pro
kimi	kimi-k3
kimi-code	k3
zhipuai-coding-plan	glm-5.3
zai	glm-5.3
zai	glm-5.3-flash
openai	gpt-6-astra
openai	gpt-5.6-sol
openai	gpt-5.6-terra
openai	gpt-5.6-luna
github-copilot	claude-opus-5
github-copilot	claude-sonnet-5
github-copilot	gemini-3.7-flash
github-copilot	gpt-5.6-sol
github-copilot	gpt-5.6-terra
github-copilot	gpt-5.6-luna
gemini	gemini-3.7-flash
gemini	gemini-3.6-flash
gemini	gemini-3.5-flash
gemini	gemini-3.1-pro-preview
EOF

# Curated free entries must remain genuinely zero-priced and tool-capable. A
# provider can keep an id while changing its pricing or supported parameters,
# so presence alone is not enough for the picker contract.
while IFS= read -r model_id; do
  jq --arg model "$model_id" -e '
    .models[$model].cost.input == 0
    and .models[$model].cost.output == 0
    and .models[$model].tool_call == true
  ' "$data_dir/openrouter.json" >/dev/null || {
    echo "Curated OpenRouter model is no longer free and tool-capable: $model_id" >&2
    exit 1
  }
done <<'EOF'
openrouter/free
cohere/north-mini-code:free
z-ai/glm-5.2:free
thinkingmachines/inkling:free
thinkingmachines/inkling-small:free
minimax/minimax-m3:free
minimax/minimax-m2.7:free
poolside/laguna-s-2.1:free
inclusionai/ling-3.0-flash-fin:free
nvidia/nemotron-3.5-lightning:free
nvidia/nemotron-3-ultra-550b-a55b:free
EOF

for model_id in gpt-6-astra gpt-5.6-sol gpt-5.6-terra gpt-5.6-luna; do
  jq --arg model "$model_id" -e '.models[$model]' \
    "$data_dir/openai-chatgpt.json" >/dev/null || {
    echo "Hand-authored ChatGPT model missing: $model_id" >&2
    exit 1
  }
done

echo "Refreshed models.dev provider slices in $data_dir"
