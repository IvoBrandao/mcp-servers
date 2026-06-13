import math
import re
from collections import Counter
from typing import List, Dict, Any, Optional

# For semantic ranking
try:
    from sentence_transformers import SentenceTransformer
    import torch

    HAS_SENTENCE_TRANSFORMERS = True
except ImportError:
    HAS_SENTENCE_TRANSFORMERS = False


class TFIDFRanker:
    def tokenize(self, text: str) -> List[str]:
        return re.findall(r"\b\w+\b", text.lower())

    def compute_tf(self, tokens: List[str]) -> Dict[str, float]:
        counter = Counter(tokens)
        max_freq = max(counter.values()) if counter else 1
        return {word: freq / max_freq for word, freq in counter.items()}

    def compute_idf(self, all_tokens: List[List[str]]) -> Dict[str, float]:
        doc_count = len(all_tokens)
        word_doc_count = Counter()
        for tokens in all_tokens:
            for word in set(tokens):
                word_doc_count[word] += 1
        return {word: math.log(doc_count / (1 + count)) for word, count in word_doc_count.items()}

    def rank(self, results: List[Dict], query: str) -> List[Dict]:
        query_tokens = self.tokenize(query)
        all_tokens = [self.tokenize(r["title"] + " " + r["snippet"]) for r in results]
        idf = self.compute_idf(all_tokens)
        for i, r in enumerate(results):
            tokens = all_tokens[i]
            tf = self.compute_tf(tokens)
            score = 0.0
            for word in query_tokens:
                if word in tf and word in idf:
                    score += tf[word] * idf[word]
            r["score"] = score
        results.sort(key=lambda x: x["score"], reverse=True)
        return results


class BM25Ranker:
    # Placeholder – implement full BM25 if needed
    def rank(self, results: List[Dict], query: str) -> List[Dict]:
        return results


class SemanticRanker:
    def __init__(self, model_name: str = "all-MiniLM-L6-v2"):
        if not HAS_SENTENCE_TRANSFORMERS:
            raise RuntimeError("sentence-transformers not installed")
        self.model = SentenceTransformer(model_name)

    def rank(self, results: List[Dict], query: str) -> List[Dict]:
        if not results:
            return results
        # Encode query and documents
        docs = [r["title"] + " " + r["snippet"] for r in results]
        query_emb = self.model.encode(query)
        doc_embs = self.model.encode(docs)
        # Cosine similarity
        similarities = torch.nn.functional.cosine_similarity(torch.tensor([query_emb]), torch.tensor(doc_embs)).tolist()
        for i, r in enumerate(results):
            r["score"] = similarities[i]
        results.sort(key=lambda x: x["score"], reverse=True)
        return results


class LLMReranker:
    # Simplified – uses OpenAI (can be extended)
    def __init__(self, model: str, api_key: Optional[str]):
        self.model = model
        self.api_key = api_key
        self.client = None
        if model.startswith("gpt"):
            import openai

            openai.api_key = api_key
            self.client = openai.AsyncOpenAI(api_key=api_key)

    async def rank(self, results: List[Dict], query: str) -> List[Dict]:
        # ... (same as previous implementation)
        return results
