# 01: Adicionar camada com altura digitada

**What to build:** Spec: `.scratch/geodata-creation/spec.md` (histórias 1–9, 13–19, 31).

O editor de geodata seleciona uma célula (pelo clique ou por Geo X/Y) e, no inspector, usa "Adicionar camada" com uma altura digitada para criar uma camada nova nessa coluna. A mensagem "A camada visível não existe nesta coluna." dá lugar a esse controle. Numa coluna que só tem `NULL_HEIGHT`, a sentinela é substituída. Numa coluna com camadas, a nova camada entra ordenada por altura e o bloco é promovido sozinho (simple → complex; complex/simple → multilayer quando necessário). A camada nasce "aberta com vínculos": abre para as vizinhas compatíveis dentro do degrau máximo e os bits opostos são espelhados nelas, como em `set_nswe`. Tudo é uma única entrada de undo/redo com rótulo "Criar camada". O combo de camadas e a tabela "Camadas da célula" passam a refletir a nova contagem sem recarregar o projeto. A nova constante de intervalo mínimo entre camadas (provisória: 32) fica ao lado do degrau máximo e do passo de altura.

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] Numa coluna `NULL_HEIGHT` de um bloco simple, adicionar camada altera só a célula alvo (as outras 63 mantêm a sentinela) e a camada fica visível e selecionável
- [ ] Numa coluna com camada, adicionar a uma altura distante deixa o bloco multilayer, com as camadas da coluna ordenadas por altura
- [ ] A altura é normalizada para o passo de 8, como em "Editar altura"; fora do intervalo editável, a operação é rejeitada
- [ ] Altura a menos do intervalo mínimo de uma camada existente, coluna no limite do formato (255 em L2J/L2G) e célula fora da grade são rejeitadas e relatadas no status
- [ ] A nova camada abre para vizinhas dentro do degrau máximo, e essas vizinhas recebem o bit oposto
- [ ] Uma única entrada de undo desfaz inserção, promoção e vínculos; redo refaz; o contador de blocos alterados e "Restaurar bloco-base" funcionam
- [ ] O combo de camadas e a tabela da célula mostram a nova camada sem recarregar o projeto
- [ ] Salvar e reabrir em `.l2j`, `.l2g` e `_conv.dat` preserva a camada, o cabeçalho do `_conv.dat` reflete a nova contagem de células e blocos não editados continuam byte a byte iguais
- [ ] A ação fica bloqueada enquanto o projeto carrega, sem cliente/geodata carregados ou com a seleção oculta, como as edições atuais
- [ ] Testes unitários na API pública do documento cobrem os cenários acima
