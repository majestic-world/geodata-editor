# 06: Mostrar células sem geodata

**What to build:** Spec: história 29.

Um novo filtro de overlay, "Mostrar células sem geodata", desenha as colunas `NULL_HEIGHT` com uma cor própria, na altura da superfície mais alta amostrada da colisão naquela coluna. Colunas sem superfície amostrada não são desenhadas. O filtro usa o mesmo caminho de instâncias e chunks 16×16 dos overlays atuais. Criar ou remover camadas atualiza só os chunks afetados.

**Blocked by:** 04 — Pincel do modo "Criar camada" acompanha a geometria

**Status:** ready-for-agent

- [ ] Com o filtro ligado, as colunas `NULL_HEIGHT` aparecem com cor própria, distinta das cores de NSWE e de seleção, nos dois temas
- [ ] Com o filtro desligado, a visualização fica igual à atual
- [ ] Depois de criar uma camada numa coluna `NULL_HEIGHT`, a marcação daquela coluna some sem reconstruir os outros chunks
- [ ] Teste de overlays: a atualização incremental, depois de inserir e remover camadas com o filtro ligado, bate com a reconstrução completa (padrão `assert_matches_full`)
