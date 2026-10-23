/**
 * Cloudflare Worker: OpenCode Go Proxy dla Cursor BYOK
 * 
 * Rozwiązuje problem routingu OpenCode Go przez:
 * - Dodanie user-agent rozpoznawanego przez OpenCode Go
 * - Stabilny x-opencode-session (fallback deterministyczny z klucza i modelu)
 * - Routing do właściwego endpoint (/inference/go/ lub /inference/)
 * - Ponawianie 429/5xx przed rozpoczęciem strumienia odpowiedzi
 * - Uzupełnianie pustych tur reasoning dla DeepSeek z narzędziami
 *   (DeepSeek wymaga reasoning_content każdej wcześniejszej tury asystenta)
 * 
 * Deploy: wrangler deploy
 * Użycie w BYOK: Server address = https://your-worker.your-subdomain.workers.dev
 */

interface Env {
  // Opcjonalnie: możesz dodać sekrety przez wrangler secret put OPENCODE_API_KEY
  OPENCODE_API_KEY?: string;
}

// Mapowanie protokołów na endpointy OpenCode
const ENDPOINT_MAP: Record<string, string> = {
  '/v1/chat/completions': '/inference/openai/v1/chat/completions',
  '/v1/responses': '/inference/openai/v1/responses',
  '/v1/messages': '/inference/anthropic/v1/messages',
};

// Modele wymagające Anthropic Messages API
const ANTHROPIC_MODELS = new Set([
  'claude-haiku-5-5',
  'claude-sonnet-5-5',
  'claude-opus-5',
  // Dodaj inne Claude modele według potrzeb
]);

// Modele wymagające OpenAI Responses API
const RESPONSES_MODELS = new Set([
  'muse-spark-1.2-contributor',
  'muse-spark-1.3-contributor',
]);

// Modele dostępne przez OpenCode Go (pełna lista z /inference/go/v1/models)
const GO_MODELS = new Set([
  'deepseek-v4.1-flash', 'deepseek-v4-pro',
  'glm-5.1', 'glm-5.2', 'glm-5.3', 'glm-5.3-flash',
  'grok-4.6', 'grok-4.7',
  'muse-spark-1.2-contributor', 'muse-spark-1.3-contributor',
  'omen-alpha',
  'gpt-5.6-luna', 'gpt-6-luna',
  'claude-haiku-5-5',
  'hy3', 'hy4-preview',
  'kimi-k2.6', 'kimi-k2.7-code', 'kimi-k3',
  'mimo-v2.5', 'mimo-v2.6-flash', 'mimo-v2.5-pro', 'mimo-v2.6-pro',
  'minimax-m2.5', 'minimax-m2.7', 'minimax-m3',
  'gemini-4-argon-contributor',
  'space-bunny',
  'longcat-2.0', 'longcat-2.5-preview-free',
  'step-5-preview',
  'beam-free',
  'qwen3.6-plus', 'qwen3.7-max', 'qwen3.8-max', 'qwen3.8-flash', 'qwen3.7-plus',
]);

const MAX_ATTEMPTS = 3;
const RETRY_DELAYS_MS = [500, 1500];
const RETRYABLE_STATUS = new Set([429, 502, 503, 504]);
const MAX_RETRY_AFTER_MS = 3_000;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Stabilna sesja, gdy klient nie wysyła x-opencode-session. */
export async function deterministicSessionId(apiKey: string, modelId: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    'SHA-256',
    new TextEncoder().encode(`${apiKey}\0${modelId}`),
  );
  const hex = Array.from(new Uint8Array(digest))
    .slice(0, 12)
    .map((byte) => byte.toString(16).padStart(2, '0'))
    .join('');
  return `ses_${hex}`;
}

/**
 * DeepSeek w trybie thinking wymaga reasoning_content każdej wcześniejszej tury
 * asystenta, gdy request zawiera narzędzia. Pusta tura reasoning przechodzi
 * walidację bramki i naprawia 400 po tool callach.
 */
export function ensureDeepSeekReasoning(body: any): boolean {
  if (!Array.isArray(body?.tools) || body.tools.length === 0) return false;
  if (typeof body.model !== 'string' || !body.model.toLowerCase().startsWith('deepseek')) {
    return false;
  }
  if (Array.isArray(body.input)) return patchResponsesInput(body);
  if (Array.isArray(body.messages)) return patchChatMessages(body);
  return false;
}

function patchResponsesInput(body: any): boolean {
  const input: any[] = [];
  let changed = false;
  for (const item of body.input) {
    if (item?.type === 'function_call' && input.at(-1)?.type !== 'reasoning') {
      input.push({
        type: 'reasoning',
        summary: [{ type: 'summary_text', text: '' }],
        content: [{ type: 'reasoning_text', text: '' }],
      });
      changed = true;
    }
    input.push(item);
  }
  if (changed) body.input = input;
  return changed;
}

function patchChatMessages(body: any): boolean {
  let changed = false;
  for (const message of body.messages) {
    if (
      message?.role === 'assistant' &&
      (message.reasoning_content === undefined || message.reasoning_content === null)
    ) {
      message.reasoning_content = '';
      changed = true;
    }
  }
  return changed;
}

function retryDelayMs(response: Response, attempt: number): number {
  const header = response.headers.get('retry-after');
  if (header) {
    const seconds = Number(header);
    if (Number.isFinite(seconds) && seconds >= 0) {
      return Math.min(seconds * 1_000, MAX_RETRY_AFTER_MS);
    }
  }
  return RETRY_DELAYS_MS[Math.min(attempt, RETRY_DELAYS_MS.length - 1)];
}

/** Ponawia tylko przed przekazaniem strumienia do klienta. */
async function fetchWithRetry(url: string, init: RequestInit): Promise<Response> {
  let response = await fetch(url, init);
  for (
    let attempt = 0;
    attempt < MAX_ATTEMPTS - 1 && RETRYABLE_STATUS.has(response.status);
    attempt++
  ) {
    try {
      await response.body?.cancel();
    } catch {
      // Ciało mogło już zostać zamknięte przez runtime.
    }
    await sleep(retryDelayMs(response, attempt));
    response = await fetch(url, init);
  }
  return response;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // CORS preflight
    if (request.method === 'OPTIONS') {
      return new Response(null, {
        headers: {
          'Access-Control-Allow-Origin': '*',
          'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
          'Access-Control-Allow-Headers':
            'Content-Type, Authorization, X-Api-Key, X-OpenCode-Session, X-OpenCode-Request, X-OpenCode-Client, X-OpenCode-Project, Anthropic-Version, Anthropic-Beta, OpenAI-Beta',
        },
      });
    }

    const url = new URL(request.url);
    
    // Health check
    if (url.pathname === '/health') {
      return new Response('OK', { status: 200 });
    }

    // Model discovery endpoint
    if (url.pathname === '/v1/models' || url.pathname.endsWith('/v1/models')) {
      try {
        const modelsResponse = await fetch('https://opencode.ai/inference/go/v1/models');
        const responseHeaders = new Headers(modelsResponse.headers);
        responseHeaders.set('Access-Control-Allow-Origin', '*');
        return new Response(modelsResponse.body, {
          status: modelsResponse.status,
          headers: responseHeaders,
        });
      } catch (error: any) {
        return new Response(
          JSON.stringify({
            error: { type: 'proxy_error', message: `Failed to fetch models: ${error.message}` },
          }),
          { status: 502, headers: { 'Content-Type': 'application/json', 'Access-Control-Allow-Origin': '*' } }
        );
      }
    }

    // Przygotuj request body
    const requestBody = await request.text();
    let body: any;
    try {
      body = JSON.parse(requestBody);
    } catch {
      return new Response('Invalid JSON body', { status: 400 });
    }

    const modelId = body.model || '';
    if (!modelId) {
      return new Response('Missing model field in request body', { status: 400 });
    }

    // DeepSeek z narzędziami: uzupełnij brakujące tury reasoning przed wysyłką.
    const reasoningPatched = ensureDeepSeekReasoning(body);
    const outboundBody = reasoningPatched ? JSON.stringify(body) : requestBody;

    // Automatycznie wybierz właściwy protokół na podstawie modelu
    let requestedPath = '';
    if (ANTHROPIC_MODELS.has(modelId)) {
      requestedPath = '/v1/messages';
    } else if (RESPONSES_MODELS.has(modelId)) {
      requestedPath = '/v1/responses';
    } else {
      // Domyślnie Chat Completions dla pozostałych modeli
      requestedPath = '/v1/chat/completions';
    }

    // Jeśli klient wyraźnie określił endpoint, użyj go (chyba że błędny dla modelu)
    const clientPath = Object.keys(ENDPOINT_MAP).find(path => url.pathname.endsWith(path));
    if (clientPath) {
      // Sprawdź zgodność protokołu z modelem
      if (ANTHROPIC_MODELS.has(modelId) && clientPath !== '/v1/messages') {
        return new Response(
          JSON.stringify({
            error: {
              type: 'protocol_mismatch',
              message: `Model ${modelId} requires Anthropic Messages API (/v1/messages), not ${clientPath}`,
            },
          }),
          { status: 400, headers: { 'Content-Type': 'application/json', 'Access-Control-Allow-Origin': '*' } }
        );
      }
      if (RESPONSES_MODELS.has(modelId) && clientPath !== '/v1/responses') {
        return new Response(
          JSON.stringify({
            error: {
              type: 'protocol_mismatch',
              message: `Model ${modelId} requires OpenAI Responses API (/v1/responses), not ${clientPath}`,
            },
          }),
          { status: 400, headers: { 'Content-Type': 'application/json', 'Access-Control-Allow-Origin': '*' } }
        );
      }
      requestedPath = clientPath;
    }

    // Określ czy model jest dostępny przez Go
    const isGoModel = GO_MODELS.has(modelId);
    
    // Wybierz endpoint: Go jeśli model jest na liście Go, standardowy w przeciwnym razie
    const targetEndpoint = isGoModel 
      ? ENDPOINT_MAP[requestedPath].replace('/inference/', '/inference/go/')
      : ENDPOINT_MAP[requestedPath];

    const targetUrl = `https://opencode.ai${targetEndpoint}`;

    // Pobierz API key z różnych headerów (Anthropic używa x-api-key, OpenAI używa Authorization)
    const authHeader = request.headers.get('Authorization');
    const apiKey = request.headers.get('x-api-key');
    
    const finalAuth = authHeader || (apiKey ? `Bearer ${apiKey}` : null) ||
      (env.OPENCODE_API_KEY ? `Bearer ${env.OPENCODE_API_KEY}` : null);
    
    if (!finalAuth) {
      return new Response(
        JSON.stringify({
          error: {
            type: 'auth_error',
            message: 'Missing Authorization or x-api-key header. Pass your OpenCode service account key from BYOK.',
          },
        }),
        {
          status: 401,
          headers: { 'Content-Type': 'application/json', 'Access-Control-Allow-Origin': '*' },
        }
      );
    }

    // Zbuduj headery dla OpenCode
    const headers = new Headers();
    headers.set('Content-Type', 'application/json');
    headers.set('Authorization', finalAuth);
    headers.set('User-Agent', 'cursor-byok/1.0 (compatible; codex)');

    // Przekaż fingerprint klienta i nagłówki protokołu; bez nich bramka Go
    // traci informacje o sesji i części modeli.
    for (const [name, value] of request.headers) {
      const lower = name.toLowerCase();
      if (
        lower.startsWith('x-opencode-') || lower === 'accept' ||
        lower === 'anthropic-version' || lower === 'anthropic-beta' ||
        lower === 'openai-beta'
      ) {
        headers.set(name, value);
      }
    }

    // Stabilna sesja: najpierw od klienta, potem deterministyczna.
    if (!headers.has('x-opencode-session')) {
      headers.set('x-opencode-session', await deterministicSessionId(finalAuth, modelId));
    }

    // Wyślij request do OpenCode
    try {
      const upstreamResponse = await fetchWithRetry(targetUrl, {
        method: 'POST',
        headers,
        body: outboundBody,
      });

      // Skopiuj response headers
      const responseHeaders = new Headers(upstreamResponse.headers);
      responseHeaders.set('Access-Control-Allow-Origin', '*');

      // Zwróć response
      return new Response(upstreamResponse.body, {
        status: upstreamResponse.status,
        statusText: upstreamResponse.statusText,
        headers: responseHeaders,
      });
    } catch (error: any) {
      return new Response(
        JSON.stringify({
          error: {
            type: 'proxy_error',
            message: `Failed to reach OpenCode: ${error.message}`,
          },
        }),
        {
          status: 502,
          headers: {
            'Content-Type': 'application/json',
            'Access-Control-Allow-Origin': '*',
          },
        }
      );
    }
  },
};
