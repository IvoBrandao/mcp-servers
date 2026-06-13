import os
from dataclasses import dataclass, field
from typing import List, Optional
from dotenv import load_dotenv

load_dotenv()


@dataclass
class EngineConfig:
    name: str
    enabled: bool = True
    api_key: Optional[str] = None
    rate_limit: float = 1.0  # requests per second
    timeout: int = 30


@dataclass
class SearchConfig:
    # General
    user_agent: str = "Mozilla/5.0 (compatible; MCPWebSearchBot/1.0)"
    max_results_per_engine: int = 10
    cache_ttl: int = 3600  # 1 hour
    concurrent_engines: int = 5

    # Web search engines
    engines: List[EngineConfig] = field(
        default_factory=lambda: [
            EngineConfig("duckduckgo", enabled=True, rate_limit=0.5),
            EngineConfig("brave", enabled=True, api_key=os.getenv("BRAVE_API_KEY")),
            EngineConfig("google", enabled=True, api_key=os.getenv("GOOGLE_API_KEY")),
            EngineConfig("bing", enabled=True, api_key=os.getenv("BING_API_KEY")),
            EngineConfig("arxiv", enabled=True),
            EngineConfig("searxng", enabled=False, api_key=os.getenv("SEARXNG_URL")),
            # New engines
            EngineConfig("yandex", enabled=True),
            EngineConfig("baidu", enabled=True),
            EngineConfig("startpage", enabled=True),
            EngineConfig("qwant", enabled=True),
        ]
    )

    # Image search engines
    image_engines: List[EngineConfig] = field(
        default_factory=lambda: [
            EngineConfig("duckduckgo_images", enabled=True, rate_limit=0.5),
            EngineConfig("google_images", enabled=True, api_key=os.getenv("GOOGLE_API_KEY")),
            EngineConfig("bing_images", enabled=True, api_key=os.getenv("BING_API_KEY")),
            EngineConfig("yandex_images", enabled=True),
            EngineConfig("baidu_images", enabled=True),
        ]
    )

    # Ranking — default to tfidf (no extra deps); use "semantic" if sentence-transformers is installed
    ranker_type: str = "tfidf"  # "tfidf", "bm25", "semantic", "llm"
    embedding_model: str = "all-MiniLM-L6-v2"  # for sentence-transformers
    llm_rerank_model: Optional[str] = None
    llm_api_key: Optional[str] = os.getenv("OPENAI_API_KEY")

    # Content fetching
    fetch_full_content: bool = True
    max_content_length: int = 10000  # characters
    fetch_timeout: int = 20
    pdf_extraction: bool = True  # enable PDF parsing


config = SearchConfig()
