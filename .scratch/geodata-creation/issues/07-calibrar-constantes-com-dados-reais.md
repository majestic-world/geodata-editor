# 07: Calibrar constantes com dados reais

**What to build:** Spec: história 46; "Further Notes" (verificação pendente).

Com um L2J e um cliente reais (variáveis de ambiente, como o teste de integração atual), um teste ignorado mede e imprime:
- como as lacunas aparecem nos arquivos reais: colunas `NULL_HEIGHT`, camadas faltando em multilayer ou células na altura do terreno com NSWE fechado;
- a distribuição dos intervalos entre camadas consecutivas nas colunas multilayer;
- a concordância da amostragem por coluna com a geodata original numa área de referência (percentual de camadas casadas e diferença de altura).

Os valores provisórios de intervalo mínimo entre camadas, espaço livre e inclinação máxima são substituídos pelos medidos. Os números e a região usada ficam registrados em `## Comments` neste ticket.

**Blocked by:** 04 — Pincel do modo "Criar camada" acompanha a geometria

**Status:** ready-for-agent

- [ ] Existe um teste ignorado que, com L2J e cliente reais, imprime as três medições acima sem salvar a geodata
- [ ] As constantes de intervalo mínimo, espaço livre e inclinação máxima refletem as medições, com uma linha de justificativa cada
- [ ] A concordância numa área de referência fica registrada neste ticket, com região e números
- [ ] Os testes unitários existentes continuam passando com os novos valores
