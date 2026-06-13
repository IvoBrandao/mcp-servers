from io import BytesIO
from typing import Optional
import aiohttp
import asyncio
from pypdf import PdfReader


class PDFParser:
    @staticmethod
    async def extract_text(url: str, session: aiohttp.ClientSession) -> Optional[str]:
        """Download PDF from URL and extract all text."""
        try:
            async with session.get(url, timeout=20) as resp:
                if resp.status != 200 or "application/pdf" not in resp.headers.get("Content-Type", ""):
                    return None
                pdf_data = await resp.read()
            reader = PdfReader(BytesIO(pdf_data))
            text = []
            for page in reader.pages:
                page_text = page.extract_text()
                if page_text:
                    text.append(page_text)
            return "\n".join(text)[:10000]  # limit length
        except Exception:
            return None
