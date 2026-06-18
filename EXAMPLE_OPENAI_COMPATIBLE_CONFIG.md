# Example OpenAI-Compatible Provider Configurations

This file shows how to configure various OpenAI-compatible providers with kimi-agent.

## Quick Start

1. Copy the relevant sections below to `~/.kimi/config.toml`
2. Set environment variables or update api_key fields
3. Run `kimi-agent --model <model-name>`

---

## DeepSeek Configuration

```toml
[models.deepseek-chat]
provider = "deepseek"
model = "deepseek-chat"
max_context_size = 64000

[models.deepseek-reasoner]
provider = "deepseek"
model = "deepseek-reasoner"
max_context_size = 64000

[providers.deepseek]
type = "open_ai_compatible"
base_url = "https://api.deepseek.com/v1"
api_key = ""  # Set OPENAI_COMPATIBLE_API_KEY env var
```

**Environment Variables:**
```bash
export OPENAI_COMPATIBLE_API_KEY="sk-..."
```

---

## OpenAI Configuration

```toml
[models.gpt-4]
provider = "openai"
model = "gpt-4"
max_context_size = 128000

[models.gpt-4-turbo]
provider = "openai"
model = "gpt-4-turbo"
max_context_size = 128000

[models.o1-preview]
provider = "openai"
model = "o1-preview"
max_context_size = 128000

[providers.openai]
type = "open_ai_compatible"
base_url = "https://api.openai.com/v1"
api_key = ""  # Set OPENAI_COMPATIBLE_API_KEY env var
```

**Environment Variables:**
```bash
export OPENAI_COMPATIBLE_API_KEY="sk-..."
export OPENAI_COMPATIBLE_TEMPERATURE="0.7"
export OPENAI_COMPATIBLE_MAX_TOKENS="4096"
```

---

## Local Model (Ollama)

```toml
[models.llama3]
provider = "local"
model = "llama3"
max_context_size = 8192

[models.mistral]
provider = "local"
model = "mistral"
max_context_size = 8192

[providers.local]
type = "open_ai_compatible"
base_url = "http://localhost:11434/v1"
api_key = "not-needed"
```

**Setup:**
```bash
# Install Ollama
curl -fsSL https://ollama.com/install.sh | sh

# Pull models
ollama pull llama3
ollama pull mistral

# Run kimi-agent
kimi-agent --model llama3
```

---

## Groq Configuration

```toml
[models.llama3-groq]
provider = "groq"
model = "llama3-70b-8192"
max_context_size = 8192

[providers.groq]
type = "open_ai_compatible"
base_url = "https://api.groq.com/openai/v1"
api_key = ""  # Set OPENAI_COMPATIBLE_API_KEY env var
```

---

## Together AI Configuration

```toml
[models.mistral-together]
provider = "together"
model = "mistralai/Mistral-7B-Instruct-v0.1"
max_context_size = 8192

[providers.together]
type = "open_ai_compatible"
base_url = "https://api.together.xyz/v1"
api_key = ""  # Set OPENAI_COMPATIBLE_API_KEY env var
```

---

## Multiple Providers Example

```toml
# Default model
default_model = "deepseek-chat"

# DeepSeek models
[models.deepseek-chat]
provider = "deepseek"
model = "deepseek-chat"
max_context_size = 64000

[models.deepseek-reasoner]
provider = "deepseek"
model = "deepseek-reasoner"
max_context_size = 64000

# OpenAI models
[models.gpt-4]
provider = "openai"
model = "gpt-4"
max_context_size = 128000

# Local models
[models.llama3-local]
provider = "local"
model = "llama3"
max_context_size = 8192

# Providers
[providers.deepseek]
type = "open_ai_compatible"
base_url = "https://api.deepseek.com/v1"
api_key = ""

[providers.openai]
type = "open_ai_compatible"
base_url = "https://api.openai.com/v1"
api_key = ""

[providers.local]
type = "open_ai_compatible"
base_url = "http://localhost:11434/v1"
api_key = "not-needed"
```

---

## Custom Headers Example

If your provider requires custom headers:

```toml
[providers.custom]
type = "open_ai_compatible"
base_url = "https://custom-api.example.com/v1"
api_key = "your-key"
custom_headers = { "X-Custom-Header" = "value" }
```

---

## Reasoning Models (DeepSeek R1, etc.)

The provider automatically handles `reasoning_content` for models that support it:

```toml
[models.deepseek-reasoner]
provider = "deepseek"
model = "deepseek-reasoner"
max_context_size = 64000
capabilities = ["thinking"]
```

No additional configuration needed - reasoning content will be displayed automatically.

---

## Tips

1. **Environment Variables**: Use env vars for API keys to avoid committing secrets
2. **Base URL**: Most providers document their base URL - check their API docs
3. **Model Names**: Use the exact model name required by the provider
4. **Context Size**: Set this to the model's actual context window size
5. **Testing**: Use `kimi-agent info` to verify your configuration

---

## Troubleshooting

**Provider not found error:**
- Check that `type = "open_ai_compatible"` is set correctly
- Verify the provider name matches between `[models.*]` and `[providers.*]`

**API key errors:**
- Verify env var `OPENAI_COMPATIBLE_API_KEY` is set
- Or set `api_key` directly in the config (not recommended for production)

**Connection errors:**
- Verify `base_url` is correct and accessible
- Check for firewalls or proxy settings
- For local models, ensure the server is running

**Model not found errors:**
- Verify the model name is correct for your provider
- Check the provider's documentation for exact model names
