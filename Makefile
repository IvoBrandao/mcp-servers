SERVERS := mcp-cc mcp-db mcp-flow mcp-fs mcp-git mcp-html-studio mcp-http mcp-py mcp-sh mcp-think mcp-ws
DIST    := dist

.PHONY: all build dist clean

# Build all servers in release mode
build:
	cargo build --release

# Build and copy binaries to dist/
dist: build
	@mkdir -p $(DIST)
	@for s in $(SERVERS); do \
		cp target/release/$$s $(DIST)/$$s; \
		echo "→ $(DIST)/$$s"; \
	done
	@echo "Done. Binaries in $(DIST)/"

# Remove dist folder and cargo build artifacts
clean:
	rm -rf $(DIST)
	cargo clean
