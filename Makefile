.PHONY: build editor tests all

EDITOR_EXE := GeodataEditor.exe

# Compila somente o editor nativo L2J.
build:
	cargo build --release --bin GeodataEditor

editor: build
	powershell -NoProfile -Command "New-Item -ItemType Directory -Path 'dist' -Force | Out-Null"
	powershell -NoProfile -Command "Copy-Item -LiteralPath 'target\release\$(EDITOR_EXE)' -Destination 'dist\$(EDITOR_EXE)' -Force"

tests:
	cargo test

# Distribuição pública: apenas o executável do editor.
all: editor
