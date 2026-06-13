import asyncio
import json
import re
import urllib.parse
from typing import List, Dict, Any, Optional
from datetime import datetime

import aiohttp
from bs4 import BeautifulSoup


# ----------------------------------------------------------------------
# Base classes
# ----------------------------------------------------------------------
class SearchEngine:
    def __init__(self, name: str, session: aiohttp.ClientSession, api_key: Optional[str] = None):
        self.name = name
        self.session = session
        self.api_key = api_key

    async def search(self, query: str, limit: int) -> List[Dict]:
        raise NotImplementedError


class ImageSearchEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        """Return list of dicts with keys: title, url, thumbnail, source_url, engine"""
        raise NotImplementedError


# ----------------------------------------------------------------------
# Web search engines (existing + new)
# ----------------------------------------------------------------------
class DuckDuckGoEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://html.duckduckgo.com/html/"
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        params = {"q": query}
        async with self.session.get(url, params=params, headers=headers) as resp:
            text = await resp.text()
        soup = BeautifulSoup(text, "html.parser")
        results = []
        for el in soup.select(".result"):
            title = el.select_one(".result__title")
            link = el.select_one(".result__url")
            snippet = el.select_one(".result__snippet")
            if title and link:
                results.append(
                    {
                        "title": title.get_text(strip=True),
                        "url": link.get("href"),
                        "snippet": snippet.get_text(strip=True) if snippet else "",
                        "engine": self.name,
                    }
                )
            if len(results) >= limit:
                break
        return results


class BraveSearchEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        if not self.api_key:
            return []
        url = "https://api.search.brave.com/res/v1/web/search"
        headers = {"Accept": "application/json", "X-Subscription-Token": self.api_key}
        params = {"q": query, "count": limit}
        async with self.session.get(url, headers=headers, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("web", {}).get("results", []):
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("url", ""),
                    "snippet": item.get("description", ""),
                    "engine": self.name,
                }
            )
        return results


class GoogleCustomSearchEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        if not self.api_key:
            return []
        cx = os.getenv("GOOGLE_CX")
        if not cx:
            return []
        url = "https://www.googleapis.com/customsearch/v1"
        params = {"key": self.api_key, "cx": cx, "q": query, "num": limit}
        async with self.session.get(url, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("items", []):
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("link", ""),
                    "snippet": item.get("snippet", ""),
                    "engine": self.name,
                }
            )
        return results


class BingSearchEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        if not self.api_key:
            return []
        url = "https://api.bing.microsoft.com/v7.0/search"
        headers = {"Ocp-Apim-Subscription-Key": self.api_key}
        params = {"q": query, "count": limit}
        async with self.session.get(url, headers=headers, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("webPages", {}).get("value", []):
            results.append(
                {
                    "title": item.get("name", ""),
                    "url": item.get("url", ""),
                    "snippet": item.get("snippet", ""),
                    "engine": self.name,
                }
            )
        return results


class ArxivEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "http://export.arxiv.org/api/query"
        params = {"search_query": query, "max_results": limit, "sortBy": "relevance"}
        async with self.session.get(url, params=params) as resp:
            text = await resp.text()
        soup = BeautifulSoup(text, "xml")
        results = []
        for entry in soup.find_all("entry")[:limit]:
            results.append(
                {
                    "title": entry.find("title").text,
                    "url": entry.find("id").text,
                    "snippet": entry.find("summary").text[:300],
                    "engine": self.name,
                }
            )
        return results


class SearxNGEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        if not self.api_key:  # api_key holds the base URL
            return []
        url = self.api_key.rstrip("/") + "/search"
        params = {"q": query, "format": "json", "limit": limit}
        async with self.session.get(url, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("results", []):
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("url", ""),
                    "snippet": item.get("content", ""),
                    "engine": self.name,
                }
            )
        return results


# New web engines (scraping‑based, no API key required)
class YandexEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://yandex.com/search/"
        params = {"text": query}
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        async with self.session.get(url, params=params, headers=headers) as resp:
            text = await resp.text()
        soup = BeautifulSoup(text, "html.parser")
        results = []
        for item in soup.select(".serp-item"):
            title_elem = item.select_one(".organic__title a")
            snippet_elem = item.select_one(".organic__text")
            if title_elem:
                results.append(
                    {
                        "title": title_elem.get_text(strip=True),
                        "url": title_elem.get("href"),
                        "snippet": snippet_elem.get_text(strip=True) if snippet_elem else "",
                        "engine": self.name,
                    }
                )
            if len(results) >= limit:
                break
        return results


class BaiduEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://www.baidu.com/s"
        params = {"wd": query}
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        async with self.session.get(url, params=params, headers=headers) as resp:
            text = await resp.text()
        soup = BeautifulSoup(text, "html.parser")
        results = []
        for item in soup.select(".result"):
            title_elem = item.select_one("h3 a")
            snippet_elem = item.select_one(".c-abstract")
            if title_elem:
                results.append(
                    {
                        "title": title_elem.get_text(strip=True),
                        "url": title_elem.get("href"),
                        "snippet": snippet_elem.get_text(strip=True) if snippet_elem else "",
                        "engine": self.name,
                    }
                )
            if len(results) >= limit:
                break
        return results


class StartpageEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://www.startpage.com/sp/search"
        params = {"query": query}
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        async with self.session.get(url, params=params, headers=headers) as resp:
            text = await resp.text()
        soup = BeautifulSoup(text, "html.parser")
        results = []
        for item in soup.select(".w-gl__result"):
            title_elem = item.select_one(".w-gl__result-title a")
            snippet_elem = item.select_one(".w-gl__result-description")
            if title_elem:
                results.append(
                    {
                        "title": title_elem.get_text(strip=True),
                        "url": title_elem.get("href"),
                        "snippet": snippet_elem.get_text(strip=True) if snippet_elem else "",
                        "engine": self.name,
                    }
                )
            if len(results) >= limit:
                break
        return results


class QwantEngine(SearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        # Qwant API is easier: https://api.qwant.com/v3/search/web
        url = "https://api.qwant.com/v3/search/web"
        params = {"q": query, "count": limit, "t": "web"}
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        async with self.session.get(url, params=params, headers=headers) as resp:
            data = await resp.json()
        results = []
        for item in data.get("data", {}).get("result", {}).get("items", []):
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("url", ""),
                    "snippet": item.get("desc", ""),
                    "engine": self.name,
                }
            )
        return results


# ----------------------------------------------------------------------
# Image search engines
# ----------------------------------------------------------------------
class DuckDuckGoImagesEngine(ImageSearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://duckduckgo.com/i.js"
        params = {"q": query, "o": "json", "p": 1, "f": ",,", "l": "us-en"}
        async with self.session.get(url, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("results", [])[:limit]:
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("image"),
                    "thumbnail": item.get("thumbnail"),
                    "source_url": item.get("url"),
                    "engine": self.name,
                }
            )
        return results


class GoogleImagesEngine(ImageSearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        # Google Custom Search API with image search enabled
        if not self.api_key:
            return []
        cx = os.getenv("GOOGLE_CX")
        if not cx:
            return []
        url = "https://www.googleapis.com/customsearch/v1"
        params = {"key": self.api_key, "cx": cx, "q": query, "searchType": "image", "num": limit}
        async with self.session.get(url, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("items", []):
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("link"),
                    "thumbnail": item.get("image", {}).get("thumbnailLink"),
                    "source_url": item.get("image", {}).get("contextLink"),
                    "engine": self.name,
                }
            )
        return results


class BingImagesEngine(ImageSearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        if not self.api_key:
            return []
        url = "https://api.bing.microsoft.com/v7.0/images/search"
        headers = {"Ocp-Apim-Subscription-Key": self.api_key}
        params = {"q": query, "count": limit}
        async with self.session.get(url, headers=headers, params=params) as resp:
            data = await resp.json()
        results = []
        for item in data.get("value", []):
            results.append(
                {
                    "title": item.get("name", ""),
                    "url": item.get("contentUrl"),
                    "thumbnail": item.get("thumbnailUrl"),
                    "source_url": item.get("hostPageUrl"),
                    "engine": self.name,
                }
            )
        return results


class YandexImagesEngine(ImageSearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://yandex.com/images/search"
        params = {"text": query, "nomisspell": 1}
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        async with self.session.get(url, params=params, headers=headers) as resp:
            text = await resp.text()
        soup = BeautifulSoup(text, "html.parser")
        results = []
        for img in soup.select(".serp-item__thumb"):
            href = img.get("href")
            if href:
                full_url = "https://yandex.com" + href
                results.append(
                    {
                        "title": query,
                        "url": full_url,  # actual image URL not directly available; use redirect
                        "thumbnail": img.get("src"),
                        "source_url": full_url,
                        "engine": self.name,
                    }
                )
            if len(results) >= limit:
                break
        return results


class BaiduImagesEngine(ImageSearchEngine):
    async def search(self, query: str, limit: int) -> List[Dict]:
        url = "https://image.baidu.com/search/index"
        params = {"tn": "baiduimage", "word": query}
        headers = {"User-Agent": "MCPWebSearchBot/1.0"}
        async with self.session.get(url, params=params, headers=headers) as resp:
            text = await resp.text()
        # Baidu returns JSON embedded in a script tag. For simplicity, we use a regex.
        import json

        pattern = r"<script.*?>.*?imgData:\s*(\{.*?\}).*?</script>"
        match = re.search(pattern, text, re.DOTALL)
        results = []
        if match:
            try:
                img_data = json.loads(match.group(1))
                for item in img_data.get("data", [])[:limit]:
                    results.append(
                        {
                            "title": item.get("desc", ""),
                            "url": item.get("thumbURL"),
                            "thumbnail": item.get("thumbURL"),
                            "source_url": item.get("fromURL"),
                            "engine": self.name,
                        }
                    )
            except:
                pass
        return results
