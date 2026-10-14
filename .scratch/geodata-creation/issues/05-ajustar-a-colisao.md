# 05: Ajustar à colisão

**What to build:** Spec: histórias 27–28; "Ajustar à colisão" na seção do documento.

O editor seleciona uma ou mais camadas (célula, pincel, retângulo ou linha) e usa "Ajustar à colisão", junto da tabela de camadas da célula. Cada camada selecionada vai para a superfície amostrada mais próxima, se a diferença estiver dentro da tolerância. As camadas sem superfície dentro da tolerância ficam como estão e são contadas no status. A ordem por altura das colunas multilayer é mantida. Uma única entrada de undo com rótulo "Ajustar à colisão".

**Blocked by:** 04 — Pincel do modo "Criar camada" acompanha a geometria

**Status:** ready-for-agent

- [ ] Uma camada deslocada alguns passos acima do piso real volta para a altura da superfície amostrada
- [ ] Uma camada sem superfície dentro da tolerância não muda e é contada como ignorada no status
- [ ] Camadas não selecionadas não mudam
- [ ] A ordem por altura das colunas multilayer continua válida depois do ajuste
- [ ] Uma única entrada de undo desfaz o ajuste de toda a seleção
- [ ] Testes unitários na API pública do documento (alturas-alvo resolvidas) e na amostragem cobrem os cenários acima
