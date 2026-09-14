.PHONY: build editor tests all

CARGO_BIN := geodata-editor
EDITOR_EXE := GeodataEditor.exe

# Compila somente o editor nativo L2J.
build:
	cargo build --release --bin $(CARGO_BIN)
	powershell -NoProfile -Command "New-Item -ItemType Directory -Path 'dist' -Force | Out-Null"
	powershell -NoProfile -Command "Copy-Item -LiteralPath 'target\release\$(CARGO_BIN).exe' -Destination 'dist\$(EDITOR_EXE)' -Force"
	powershell -NoProfile -Command "Copy-Item -LiteralPath 'LICENSE' -Destination 'dist\LICENSE' -Force"

tests:
	cargo test
