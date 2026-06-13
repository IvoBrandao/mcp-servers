SERVERS := mcp-cc mcp-flow mcp-fs mcp-html-studio mcp-py mcp-sh mcp-think mcp-ws

.PHONY: sync clean reset $(SERVERS)

# Sync all venvs (fast — skips servers where nothing changed)
sync:
	@for s in $(SERVERS); do \
		echo "→ $$s"; \
		(cd $$s && uv sync 2>&1 | grep -v "^Resolved\|^Checked\|^Audited" || true); \
	done
	@echo "Done."

# Remove all .venv dirs and __pycache__ trees
clean:
	@for s in $(SERVERS); do \
		echo "→ $$s"; \
		rm -rf $$s/.venv; \
		find $$s -type d -name __pycache__ -exec rm -rf {} + 2>/dev/null; \
		find $$s -name "*.pyc" -delete 2>/dev/null; \
	done
	@echo "Done."

# Full rebuild: clean then sync
reset: clean sync
