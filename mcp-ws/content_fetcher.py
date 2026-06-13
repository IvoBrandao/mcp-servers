import asyncio
import hashlib
import time
from typing import Optional
import aiohttp
from bs4 import BeautifulSoup
import trafilatura
from cachetools import TTLCache
from pdf_parser import PDFParser


class ContentFetcher:
    def __init__(self, cache_ttl: int = 3600, max_size: int = 1000, enable_pdf: bool = True):
        self.cache = TTLCache(maxsize=max_size, ttl=cache_ttl)
        self.session = None
        self.rate_limit = 1.0
        self._last_request = 0
        self.enable_pdf = enable_pdf

    async def get(self, url: str, timeout: int = 20) -> Optional[str]:
        # Rate limiting
        now = time.time()
        if now - self._last_request < self.rate_limit:
            await asyncio.sleep(self.rate_limit - (now - self._last_request))
        self._last_request = time.time()

        # Cache
        key = hashlib.md5(url.encode()).hexdigest()
        if key in self.cache:
            return self.cache[key]

        if not self.session:
            self.session = aiohttp.ClientSession()

        # Check if PDF
        if self.enable_pdf and url.lower().endswith(".pdf"):
            text = await PDFParser.extract_text(url, self.session)
            if text:
                self.cache[key] = text
                return text

        # Fetch HTML
        try:
            async with self.session.get(url, timeout=timeout, headers={"User-Agent": "MCPWebSearchBot/1.0"}) as resp:
                if resp.status != 200:
                    return None
                html = await resp.text()
        except Exception:
            return None

        # Extract main content
        text = trafilatura.extract(html, include_comments=False, include_tables=False)
        if text:
            text = text[:10000]
            self.cache[key] = text
            return text
        return None
