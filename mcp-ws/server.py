"""
MCP Web Search Server — FastMCP edition
"""

import asyncio
import os
from typing import Any, Dict, List, Optional
from contextlib import asynccontextmanager

import aiohttp
from mcp.server.fastmcp import FastMCP, Context

from config import config
from search_engines import (
    DuckDuckGoEngine,
    BraveSearchEngine,
    GoogleCustomSearchEngine,
    BingSearchEngine,
    ArxivEngine,
    SearxNGEngine,
    YandexEngine,
    BaiduEngine,
    StartpageEngine,
    QwantEngine,
    DuckDuckGoImagesEngine,
    GoogleImagesEngine,
    BingImagesEngine,
    YandexImagesEngine,
    BaiduImagesEngine,
)
from ranker import TFIDFRanker, BM25Ranker, SemanticRanker, LLMReranker
from content_fetcher import ContentFetcher

# ----------------------------------------------------------------------
# Global objects (initialised in lifespan)
# ----------------------------------------------------------------------
_session: Optional[aiohttp.ClientSession] = None
_fetcher: Optional[ContentFetcher] = None
_ranker = None  # cached ranker instance

WEB_ENGINE_CLASSES = {
    "duckduckgo": DuckDuckGoEngine,
    "brave": BraveSearchEngine,
    "google": GoogleCustomSearchEngine,
    "bing": BingSearchEngine,
    "arxiv": ArxivEngine,
    "searxng": SearxNGEngine,
    "yandex": YandexEngine,
    "baidu": BaiduEngine,
    "startpage": StartpageEngine,
    "qwant": QwantEngine,
}
IMAGE_ENGINE_CLASSES = {
    "duckduckgo_images": DuckDuckGoImagesEngine,
    "google_images": GoogleImagesEngine,
    "bing_images": BingImagesEngine,
    "yandex_images": YandexImagesEngine,
    "baidu_images": BaiduImagesEngine,
}


def _build_ranker():
    if config.ranker_type == "tfidf":
        return TFIDFRanker()
    elif config.ranker_type == "bm25":
        return BM25Ranker()
    elif config.ranker_type == "semantic":
        return SemanticRanker(model_name=config.embedding_model)
    elif config.ranker_type == "llm":
        return LLMReranker(config.llm_rerank_model, config.llm_api_key)
    else:
        raise ValueError(f"Unknown ranker type: {config.ranker_type}")


# ----------------------------------------------------------------------
# Lifespan — create / close the HTTP session and ranker
# ----------------------------------------------------------------------
@asynccontextmanager
async def lifespan(app):
    global _session, _fetcher, _ranker
    _session = aiohttp.ClientSession()
    _fetcher = ContentFetcher(cache_ttl=config.cache_ttl, enable_pdf=config.pdf_extraction)
    try:
        _ranker = _build_ranker()
    except Exception as e:
        # Graceful fallback if optional ranker deps are missing
        _ranker = TFIDFRanker()
    yield
    await _session.close()


# ----------------------------------------------------------------------
# MCP server
# ----------------------------------------------------------------------
mcp = FastMCP("mcp-ws", lifespan=lifespan)


@mcp.tool()
async def web_search(
    query: str,
    limit: int = 10,
    fetch_content: bool = True,
    stream: bool = False,
    ctx: Context = None,
) -> str:
    """Search the web using multiple engines (DuckDuckGo, Brave, Google, Bing, ArXiv, SearxNG,
    Yandex, Baidu, Startpage, Qwant). Returns ranked, deduplicated results with optional
    full-page content extraction."""
    if not query:
        return "❌ Missing 'query'."

    engines = []
    for eng_cfg in config.engines:
        if not eng_cfg.enabled:
            continue
        if eng_cfg.name not in WEB_ENGINE_CLASSES:
            continue
        if eng_cfg.name in ("brave", "google", "bing") and not eng_cfg.api_key:
            continue
        engines.append(WEB_ENGINE_CLASSES[eng_cfg.name](eng_cfg.name, _session, api_key=eng_cfg.api_key))

    if not engines:
        return "❌ No web search engines enabled."

    async def search_one(engine):
        try:
            return await engine.search(query, limit)
        except Exception:
            return []

    results_lists = await asyncio.gather(*[search_one(e) for e in engines])

    seen_domains: set = set()
    all_results = []
    for lst in results_lists:
        for r in lst:
            domain = r["url"].split("/")[2] if "://" in r["url"] else r["url"]
            if domain not in seen_domains:
                seen_domains.add(domain)
                all_results.append(r)

    if not all_results:
        return "No results found."

    if isinstance(_ranker, LLMReranker):
        all_results = await _ranker.rank(all_results, query)
    else:
        all_results = _ranker.rank(all_results, query)

    if fetch_content:
        for i, res in enumerate(all_results[:5]):
            content = await _fetcher.get(res["url"])
            if content:
                all_results[i]["full_content"] = content

    lines = []
    for i, res in enumerate(all_results[:limit], 1):
        line = (
            f"{i}. **{res['title']}**\n"
            f"   URL: {res['url']}\n"
            f"   Engine: {res['engine']}\n"
            f"   Snippet: {res['snippet']}"
        )
        if "score" in res:
            line += f"\n   Score: {res['score']:.4f}"
        if "full_content" in res:
            line += f"\n   Content: {res['full_content'][:500]}..."
        lines.append(line)

    result_text = "\n\n".join(lines)

    if stream and ctx:
        chunk_size = 4096
        for i in range(0, len(result_text), chunk_size):
            ctx.info(result_text[i: i + chunk_size])
        return "[Streaming complete]"

    return result_text


@mcp.tool()
async def image_search(query: str, limit: int = 10) -> str:
    """Search for images using DuckDuckGo, Google, Bing, Yandex, and Baidu.
    Returns image URLs, thumbnails, and source pages."""
    if not query:
        return "❌ Missing 'query'."

    engines = []
    for eng_cfg in config.image_engines:
        if not eng_cfg.enabled:
            continue
        if eng_cfg.name not in IMAGE_ENGINE_CLASSES:
            continue
        if eng_cfg.name in ("google_images", "bing_images") and not eng_cfg.api_key:
            continue
        engines.append(IMAGE_ENGINE_CLASSES[eng_cfg.name](eng_cfg.name, _session, api_key=eng_cfg.api_key))

    if not engines:
        return "❌ No image search engines enabled."

    async def search_one(engine):
        try:
            return await engine.search(query, limit)
        except Exception:
            return []

    results_lists = await asyncio.gather(*[search_one(e) for e in engines])

    seen_urls: set = set()
    all_results = []
    for lst in results_lists:
        for r in lst:
            if r["url"] not in seen_urls:
                seen_urls.add(r["url"])
                all_results.append(r)

    if not all_results:
        return "No images found."

    lines = []
    for i, res in enumerate(all_results[:limit], 1):
        lines.append(
            f"{i}. **{res['title']}**\n"
            f"   Image URL: {res['url']}\n"
            f"   Thumbnail: {res['thumbnail']}\n"
            f"   Source: {res['source_url']}\n"
            f"   Engine: {res['engine']}"
        )
    return "\n\n".join(lines)


@mcp.tool()
async def fetch_page(url: str) -> str:
    """Fetch and extract the main textual content from a URL (HTML or PDF)."""
    if not url:
        return "❌ Missing 'url'."
    content = await _fetcher.get(url)
    if content:
        return content[:10000]
    return "Failed to fetch or extract content."


# ----------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------
if __name__ == "__main__":
    mcp.run()
