# OpenCode Go Proxy dla Cursor BYOK

Cloudflare Worker rozwiązujący problem routingu OpenCode Go w zewnętrznych klientach.

## Problem

OpenCode Go wymaga:
1. User-agent rozpoznawanego jako zwalidowany klient (np. `codex`)
2. Header `x-opencode-session` dla optymalizacji routingu
3. Endpoint `/inference/go/...` zamiast standardowego `/inference/...`
4. Cursor BYOK wysyła `claude-cli/2.1.177`, który OpenCode odrzuca

## Rozwiązanie

Worker działa jako proxy:
- Dodaje user-agent: `cursor-byok/1.0 (compatible; codex)`
- Przekazuje `x-opencode-session` od klienta; jeśli go brak, wylicza stabilny
  identyfikator z klucza API i modelu (retry nie tworzy nowej sesji)
- Automatycznie wybiera endpoint (`/go/` dla modeli Go, `/standard/` dla reszty)
- Ponawia 429/502/503/504 (maks. 2 razy) przed rozpoczęciem streamu
- Dla DeepSeek z narzędziami uzupełnia puste tury `reasoning`, których wymaga
  thinking mode (inaczej bramka zwraca 400 po tool callach)
- Obsługuje streaming responses
- Przekazuje wszystkie błędy upstream

## Deploy

```bash
cd support/cloudflare-worker

# Zainstaluj wrangler (jeśli nie masz)
npm install

# Zaloguj się do Cloudflare
npx wrangler login

# Deploy worker
npm run deploy
```

Worker będzie dostępny pod: `https://opencode-go-proxy.your-subdomain.workers.dev`

### Opcjonalnie: dodaj API key jako secret

Jeśli chcesz, żeby worker automatycznie dodawał klucz API (nie musisz go przesyłać z BYOK):

```bash
npx wrangler secret put OPENCODE_API_KEY
# Wklej swój klucz service account (sk_...)
```

## Konfiguracja w Cursor BYOK

### Wariant 1: API key w workerze (bezpieczniejsze)

```
Model type: OpenAI
Request protocol: Responses API
Server address: https://opencode-go-proxy.your-subdomain.workers.dev
☐ Use complete request URL
API Key: (zostaw puste)
Model name: muse-spark-1.3
Custom Headers:
{
  "x-opencode-session": "{{SessionId}}"
}
```

### Wariant 2: API key z BYOK

```
Model type: OpenAI
Request protocol: Responses API
Server address: https://opencode-go-proxy.your-subdomain.workers.dev
☐ Use complete request URL
API Key: sk_twoj_klucz_service_account
Model name: muse-spark-1.3
Custom Headers:
{
  "x-opencode-session": "{{SessionId}}"
}
```

## Modele OpenCode Go

Worker automatycznie rozpoznaje modele Go i wybiera właściwy endpoint:

**Muse Spark (Meta Llama):**
- `muse-spark-1.3`
- `muse-spark-1.2`

**GPT (OpenAI):**
- `gpt-6-luna`
- `gpt-5.6-luna`

**Claude (Anthropic):**
- `claude-haiku-5-5`

**Gemini (Google):**
- `gemini-4-argon`

**GLM (Zhipu):**
- `glm-5.3-flash`
- `glm-5.3`
- `glm-5.2`

**Kimi (Moonshot):**
- `kimi-k3`
- `kimi-k2.7-code`

**DeepSeek:**
- `deepseek-v4.1-flash`
- `deepseek-v4-pro`

**MiniMax:**
- `minimax-m3`

**Qwen (Alibaba):**
- `qwen3.8-max`
- `qwen3.8-flash`
- `qwen3.7-plus`

**Grok (xAI):**
- `grok-4.7`

## Monitoring

```bash
# Zobacz logi w czasie rzeczywistym
npm run tail

# Sprawdź health check
curl https://opencode-go-proxy.your-subdomain.workers.dev/health
```

## Troubleshooting

### 401 Unauthorized
- Worker nie ma `OPENCODE_API_KEY` w secrets
- Nie przekazałeś `Authorization: Bearer sk_...` z BYOK
- Klucz jest nieprawidłowy lub wygasł

### 400 `The reasoning_content in the thinking mode must be passed back to the API`
- DeepSeek z narzędziami wymaga historii reasoning każdej wcześniejszej tury
  asystenta. Worker uzupełnia brakujące tury pustym `reasoning`; jeśli błąd
  nadal występuje, zaktualizuj worker (`npm run deploy`) albo wyłącz thinking.
- Ten błąd nie ma związku z rozmiarem requestu: 400–600 KB requesty przechodzą.

### 429 `Upstream request failed: Endpoint is unavailable`
- To chwilowa niedostępność bramki OpenCode Go, nie limit planu. Worker
  ponawia automatycznie; przy dłuższych awariach sprawdź
  https://status.opencode.ai.

### 400 Model is unavailable
- Model nie jest dostępny w Twoim planie Go
- Sprawdź listę modeli w OpenCode Console

### 402 Payment Required
- Wyczerpałeś limit użycia Go
- Sprawdź usage w https://opencode.ai/console

### 502 Bad Gateway
- OpenCode API jest niedostępne
- Sprawdź status: https://status.opencode.ai
