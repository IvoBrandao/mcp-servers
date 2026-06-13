# mcp-ws — Web Search Server

Multi-engine web search, image search, and page fetching for the Model Context Protocol.

## Features

- **10+ web search engines** — DuckDuckGo, Brave, Google, Bing, ArXiv, SearxNG, Yandex, Baidu, Startpage, Qwant
- **5 image search engines** — DuckDuckGo, Google, Bing, Yandex, Baidu
- **Ranking** — TF-IDF (default), BM25, semantic (`sentence-transformers`), or LLM reranking
- **Full-page content extraction** — HTML via trafilatura, PDF via PyPDF
- **Concurrent queries** — all enabled engines run in parallel
- **Deduplication** — domain-level grouping removes duplicate results
- **Caching** — search results and page content are cached (default 1 hour TTL)
- **Rate limiting** — per-engine request throttling

## Installation

```bash
cd mcp-ws
uv sync
# Optional: semantic reranking
uv pip install sentence-transformers torch
```

Copy `.env.example` to `.env` and add your API keys:

```bash
cp .env.example .env
```

## Configuration

Edit `config.py` to:
- Enable/disable individual engines
- Change the ranking algorithm (`tfidf`, `bm25`, `semantic`, `llm`)
- Adjust rate limits, timeouts, and cache TTL

Engines that require an API key (Brave, Google, Bing) are automatically skipped if the key is not set.

### Environment variables

| Variable | Description |
|----------|-------------|
| `BRAVE_API_KEY` | Brave Search API key |
| `GOOGLE_API_KEY` | Google Custom Search API key |
| `BING_API_KEY` | Bing Search API key |
| `SEARXNG_URL` | URL of your SearxNG instance |
| `OPENAI_API_KEY` | OpenAI API key (for LLM reranking only) |

## Usage

```bash
uv run server.py
```

## MCP Tools

| Tool | Description |
|------|-------------|
| `web_search` | Search the web across all enabled engines; returns ranked, deduplicated results |
| `image_search` | Search for images; returns URLs, thumbnails, and source pages |
| `fetch_page` | Extract the main textual content from a URL (HTML or PDF) |

### `web_search` parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `query` | required | Search query |
| `limit` | 10 | Maximum number of results to return |
| `fetch_content` | true | Fetch and attach content from top 5 results |
| `stream` | false | Stream results via MCP log messages |

## Claude Desktop Configuration

```json
{
  "mcpServers": {
    "mcp-ws": {
      "command": "uv",
      "args": ["--directory", "/absolute/path/to/mcp-ws", "run", "server.py"]
    }
  }
}
```

## License

MIT
