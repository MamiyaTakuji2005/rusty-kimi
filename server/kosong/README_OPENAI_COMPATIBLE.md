# OpenAI-Compatible Provider

The `openai_compatible` provider allows dvadva-agent to work with any LLM API that follows the OpenAI chat completions format.

## Supported Providers

- **OpenAI** (GPT-4, GPT-4 Turbo, o1, o3)
- **DeepSeek** (V3, R1, V4)
- **Groq** (LPU inference)
- **Together AI** (open-source models)
- **Local models** (Ollama, LM Studio, vLLM, etc.)
- **Any other OpenAI-compatible endpoint**

## Configuration

Add to your `~/.kimi/config.toml`:

```toml
[models.deepseek-chat]
provider = "deepseek"
model = "deepseek-chat"
max_context_size = 64000

[providers.deepseek]
type = "open_ai_compatible"
base_url = "https://api.deepseek.com/v1"
api_key = ""  # Will use OPENAI_COMPATIBLE_API_KEY env var

[models.gpt4]
provider = "openai"
model = "gpt-4"
max_context_size = 128000

[providers.openai]
type = "open_ai_compatible"
base_url = "https://api.openai.com/v1"
api_key = ""  # Will use OPENAI_COMPATIBLE_API_KEY env var

[models.local-llama]
provider = "local"
model = "llama3"
max_context_size = 8192

[providers.local]
type = "open_ai_compatible"
base_url = "http://localhost:11434/v1"
api_key = "not-needed"
```

## Environment Variables

- `OPENAI_COMPATIBLE_API_KEY` - API key for the provider
- `OPENAI_COMPATIBLE_BASE_URL` - Override base URL
- `OPENAI_COMPATIBLE_MODEL_NAME` - Override model name
- `OPENAI_COMPATIBLE_MAX_CONTEXT_SIZE` - Override context size
- `OPENAI_COMPATIBLE_TEMPERATURE` - Set temperature
- `OPENAI_COMPATIBLE_TOP_P` - Set top_p
- `OPENAI_COMPATIBLE_MAX_TOKENS` - Set max tokens

## Usage

```bash
# Use DeepSeek
dvadva-agent --model deepseek-chat

# Use OpenAI
dvadva-agent --model gpt4

# Use local model
dvadva-agent --model local-llama
```

## Features

✅ Streaming support
✅ Reasoning content (for DeepSeek R1, etc.)
✅ Tool/function calling
✅ Token usage tracking
✅ Custom headers
✅ Generation kwargs (temperature, top_p, etc.)
