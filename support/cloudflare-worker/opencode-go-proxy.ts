/**
 * Cloudflare Worker: OpenCode Go Proxy dla Cursor BYOK
 * 
 * Rozwiązuje problem routingu OpenCode Go przez:
 * - Dodanie user-agent rozpoznawanego przez OpenCode Go
 * - Przekazanie x-opencode-session header
 * - Routing do właściwego endpoint (/inference/go/ lub /inference/)
 * - Transparentne proxy wszystkich modeli
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

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // CORS preflight
    if (request.method === 'OPTIONS') {
      return new Response(null, {
        headers: {
          'Access-Control-Allow-Origin': '*',
          'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
          'Access-Control-Allow-Headers': 'Content-Type, Authorization, X-OpenCode-Session',
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
    
    const finalAuth = authHeader || (apiKey ? `Bearer ${apiKey}` : null);
    
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
    
    // Przekaż x-opencode-session jeśli jest w request
    const sessionId = request.headers.get('X-OpenCode-Session') || 
                      request.headers.get('x-opencode-session');
    if (sessionId) {
      headers.set('x-opencode-session', sessionId);
    } else {
      // Wygeneruj losowy session ID jeśli brak
      const randomSession = `ses_${crypto.randomUUID().replace(/-/g, '')}`;
      headers.set('x-opencode-session', randomSession);
    }

    // Skopiuj inne istotne headery
    const relevantHeaders = ['anthropic-beta', 'openai-beta'];
    for (const h of relevantHeaders) {
      const val = request.headers.get(h);
      if (val) headers.set(h, val);
    }

    // Wyślij request do OpenCode
    try {
      const upstreamResponse = await fetch(targetUrl, {
        method: 'POST',
        headers,
        body: requestBody,
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
