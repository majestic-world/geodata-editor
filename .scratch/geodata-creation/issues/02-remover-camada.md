# 02: Remover camada

**What to build:** Spec: histórias 10–16, 20.

O editor seleciona uma camada na tabela "Camadas da célula" e usa "Remover camada". A camada some da coluna. Se era a última, a coluna vira `NULL_HEIGHT` com NSWE fechado, porque o formato não aceita coluna vazia. Os bits das vizinhas que apontavam para a camada removida são fechados. Uma única entrada de undo/redo com rótulo "Remover camada"; o combo e a tabela de camadas se atualizam.

**Blocked by:** 01 — Adicionar camada com altura digitada

**Status:** ready-for-agent

- [ ] Remover uma camada de uma coluna multilayer mantém as demais camadas ordenadas
- [ ] Remover a última camada de uma coluna deixa `NULL_HEIGHT` com NSWE fechado e a coluna some da viewport
- [ ] As vizinhas que tinham o bit oposto apontando para a camada removida ficam com esse bit fechado
- [ ] Uma única entrada de undo restaura exatamente o estado anterior, incluindo os vínculos das vizinhas
- [ ] O combo de camadas e a tabela da célula se atualizam, e a camada visível selecionada continua válida
- [ ] Salvar e reabrir nos 3 formatos preserva a remoção
- [ ] Testes unitários na API pública do documento cobrem os cenários acima
