# Auditoria de áudio — ZapExt vs ZapFast 0.15

## Problema observado
Em 1.5x e 2x a voz fica fina/"esquilo".

## Causa no ZapExt
O player atual usa `rodio::Player::set_speed(speed)`. Isso muda a velocidade por resampling. Ao reproduzir amostras mais rápido, a frequência fundamental sobe junto; portanto pitch e formantes sobem.

O próprio comentário atual do fork afirma que esse comportamento é esperado. Para UX de mensagens de voz, não é o comportamento desejável.

## Implementação upstream
O ZapFast 0.15 adicionou `src/timestretch.rs` com WSOLA (Waveform Similarity Overlap-Add):
- FRAME 2048 (~43 ms a 48 kHz);
- hop de síntese 512;
- busca de alinhamento ±448 amostras;
- comparação decimada em passo 8;
- janela sin²;
- job em background cancelável;
- cache por fator;
- sink continua tocando a 1x e recebe uma cópia temporalmente comprimida.

Há testes para:
- duração /1.5 e /2;
- tom de 440 Hz permanecer aproximadamente 440 Hz;
- loudness;
- silêncio;
- cancelamento.

## Veredito
Para qualidade de voz, o upstream 0.15 é melhor.

## Como integrar sem perder vantagens do fork
Não copiar `audio.rs` inteiro. O ZapExt possui vantagens que devem ser preservadas:
- spool em disco para clips longos;
- decode em blocos;
- memória limitada;
- velocidade por mensagem;
- autoplay de próximo áudio;
- liberação do device/dados após finalizar.

### Arquitetura proposta
1. adicionar `src/timestretch.rs` adaptado;
2. manter decode/spool atual;
3. para clips pequenos em memória: gerar stretch WSOLA completo em background;
4. para clips grandes/spooled: usar processamento por janelas/segmentos ou cache temporário comprimido em spool, para não recolocar clips longos inteiros em RAM;
5. cachear 1.5x e 2x por mensagem enquanto carregada;
6. cancelar worker quando mensagem/speed muda;
7. manter progresso no tempo original;
8. sink sempre em 1x quando usando buffer WSOLA.

## UX recomendada
- clique 1x -> 1.5x -> 2x -> 1x;
- mudança rápida sem salto de progresso;
- enquanto o stretch ainda prepara, continuar na velocidade anterior e exibir estado discreto, sem congelar;
- nunca alterar pitch para voz.

## Testes a adicionar
- seno 440 Hz em 1.5x e 2x;
- fala real curta;
- clip longo spooled;
- troca 1x->2x->1.5x durante playback;
- pause/resume durante build;
- seek durante build;
- autoplay;
- cancelamento de job antigo;
- limite de memória.

## Prioridade
P0.
