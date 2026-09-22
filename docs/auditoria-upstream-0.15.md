# Auditoria upstream ZapFast 0.15.0 vs ZapExt 1.0.38

Data: 2026-09-22

## Escopo
Comparação do ZapFast oficial v0.14.0 -> v0.15.0 com o fork ZapExt atual na main, priorizando funcionalidade, UX, algoritmos e regressões.

## Resumo executivo
O upstream 0.15.0 traz 54 commits sobre 0.14.0. Nem tudo deve ser portado literalmente: o ZapExt já avançou muito além do upstream em vídeo, PDFs, stickers, cache e fluxo de mídia. A estratégia recomendada é cherry-pick conceitual/port manual por recurso.

### Prioridade P0 — portar/adaptar
1. Time-stretch WSOLA para áudio 1.5x/2x, preservando pitch.
2. Diagnóstico e correção do fluxo Windows tray/taskbar/restore.
3. Assinatura criptográfica de updates.
4. Filtros de chat por não lidas, privadas e grupos.
5. Picker completo de reações.
6. Chats bloqueados sincronizados e excluídos de busca/notificações.

### Prioridade P1
- acessibilidade/foco por teclado;
- Rosé Pine;
- correções de exclusão/clear-chat e proteção contra histórico atrasado restaurar mensagens;
- robustez de nomes de grupos;
- staging/timeout explícito de downloads;
- refinamentos de estados de erro.

## Comparativo de recursos

| Recurso upstream 0.15 | Estado no ZapExt | Decisão |
|---|---|---|
| Filtros Unread/Private/Groups | ausente; há Chats/Channels e Archived | portar |
| Picker completo de reações | não equivalente ao upstream | portar/adaptar ao picker existente |
| Áudio 1x/1.5x/2x | existe | substituir algoritmo de aceleração |
| Duplo clique para responder | não confirmado no fork | portar se não conflitar com seleção de texto |
| Rosé Pine/Moon/Dawn | ausente | portar |
| Acessibilidade + foco por Tab | parcial | portar infraestrutura de foco |
| Nix flake | ausente | opcional |
| Correção RTL/árabe | fork possui bidi próprio; comparar testes | testar antes de portar |
| Clear/delete sync robusto | implementação própria, sem garantia equivalente | portar testes/regras |
| Locked chats | ausente | portar |
| Nomes de grupos resilientes | fork já possui retry/backoff de metadata | manter fork e comparar edge cases |
| Anexos 64 MiB + timeout 2 min + staging | há limite automático de 64 MiB, mas não equivalente completo | portar timeout/staging/status |
| Browser image paste sem URL | não confirmado | portar teste |
| Erros persistentes/Not sent/contraste | parcial | adaptar |
| Redução de idle redraw | fork tem otimizações próprias de animação | benchmark antes de copiar |
| Archived sem notificações | já presente: maybe_notify ignora archived | manter fork |
| BR phone formatting | já presente | manter fork |
| Business pairing update | dependência/worker próprios | comparar versão da whatsapp-rust |
| Signed updates | checksum existe, assinatura pública não | portar |

## O que o ZapExt já faz melhor
- Player de vídeo in-app, com H.264 via openh264 e fallback ffmpeg.
- Seek por keyframe, inclusive gaps longos e múltiplos seeks concorrentes.
- Sincronização áudio/vídeo e fallback de relógio.
- Controle de volume/mute e persistência.
- Poster real e duração obtida do arquivo.
- Viewer PDF com cache/prefetch.
- Pipeline de stickers com spool, orçamento de memória, retry e deduplicação.
- Autoplay de áudios seguintes.
- Cache/arquivamento mais extensos.
- Handling de mídia e recuperação de falhas mais sofisticados.

## Regra de integração
Não substituir módulos inteiros do fork por upstream 0.15. Portar apenas algoritmos, testes e invariantes relevantes. O fork já divergiu funcionalmente em áreas críticas.
