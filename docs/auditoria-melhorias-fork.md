# Backlog de melhorias do ZapExt após auditoria 0.15

## P0
- [ ] Portar WSOLA pitch-preserving para 1.5x/2x.
- [ ] Corrigir ciclo Windows tray/taskbar/restore.
- [ ] Adicionar assinatura Ed25519 (ou mecanismo equivalente do upstream) aos updates além de SHA-256.
- [ ] Filtros Unread / Private / Groups com contadores.
- [ ] Picker completo de reações com busca + recentes.
- [ ] Locked chats: ocultar de lista normal, busca e notificações; pasta protegida local.

## P1
- [ ] Infra de acessibilidade/foco por teclado.
- [ ] Rosé Pine / Moon / Dawn.
- [ ] Double-click em área vazia do bubble para reply sem quebrar seleção de texto.
- [ ] Portar testes de clear-chat para impedir ressurreição via history sync.
- [ ] Garantir que metadata vazia nunca apague nome de grupo válido.
- [ ] Download staging + timeout explícito + retry inline.
- [ ] Pasting de imagem do navegador sem URL de origem.
- [ ] Melhorar estado "Not sent" e ações de erro.
- [ ] Contraste/estado de switches independente de cor.

## P2
- [ ] Nix flake.
- [ ] Hardware video decode opcional.
- [ ] resolução/buffer adaptativos de vídeo.
- [ ] benchmark idle CPU/GPU.
- [ ] testes reais com screen readers.

## Itens do fork a manter
- vídeo in-app avançado;
- seek por keyframe;
- poster/duração reais;
- fallbacks Symphonia/ffmpeg;
- viewer PDF;
- stickers/cache/spool;
- autoplay de voice notes;
- orçamento de memória de animações;
- segurança ao não executar anexos/programas;
- melhorias de notificações e links para mensagens;
- deduplicação e retry de mídia.

## Critério de aceite
Uma mudança upstream só entra se:
1. resolve bug não resolvido no fork; ou
2. melhora UX mensuravelmente; ou
3. reduz CPU/memória/latência; ou
4. aumenta compatibilidade/segurança sem remover recursos do fork.

Quando houver implementação concorrente, manter testes A/B e escolher por:
- qualidade perceptual;
- p95 de latência;
- CPU;
- memória;
- estabilidade;
- simplicidade de manutenção.
