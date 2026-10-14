# 10: Validação ponta a ponta com dados reais e README

**What to build:** Spec: história 47; "Testing Decisions" (integração real).

Um teste ignorado com L2J e cliente reais gera numa área, aplica, salva num arquivo temporário, reabre, valida e confirma que o arquivo de origem não mudou. O README passa a documentar o modo "Criar camada", "Adicionar/Remover camada", "Ajustar à colisão", o filtro de células sem geodata e a geração por área com prévia, incluindo os limites (sem remoção automática, blocos alterados fora por padrão, limite da seleção). Smoke manual no app real: criar camada numa ponte, adicionar e remover pelo inspector, ajustar à colisão, gerar numa área pequena, conferir a prévia, aplicar, desfazer e salvar.

**Blocked by:** 02 — Remover camada; 06 — Mostrar células sem geodata; 09 — Recalcular NSWE na geração

**Status:** ready-for-agent

- [ ] O teste ignorado de geração passa com L2J e cliente reais, e o arquivo de origem continua byte a byte igual
- [ ] O README descreve cada fluxo novo e seus limites
- [ ] O resultado do smoke manual fica registrado em `## Comments` neste ticket
