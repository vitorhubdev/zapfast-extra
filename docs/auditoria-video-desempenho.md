# Auditoria de vídeo e desempenho

## Estado atual
O ZapExt está significativamente à frente do upstream 0.15 em vídeo.

### Pontos fortes existentes
- playback in-app;
- demux MP4 + openh264;
- fallback ffmpeg;
- áudio via rodio e extração por Symphonia/ffmpeg;
- seek por keyframe;
- suporte a gaps grandes entre keyframes;
- cancelamento por geração;
- buffer limitado;
- textura única atualizada em vez de uma textura por frame;
- poster real;
- duração real;
- fallback para formatos não suportados;
- tratamento de pausa/seek/sync.

## Onde ainda pode melhorar

### 1. Decoding resolution adaptativa
Hoje PLAY_WIDTH é 480. É eficiente, porém fixo.
Sugestão:
- 360p quando viewer pequeno/CPU pressionada;
- 480p padrão;
- 720p quando viewer grande e decoder sustenta frame budget;
- reduzir dinamicamente se frame lateness ultrapassar limiar.

### 2. Frame pacing
Usar deadline por PTS e solicitar repaint no próximo frame real, evitando repaint periódico acima do necessário.
Métrica: lateness = now_clock - frame_pts.

### 3. Drop inteligente de frames
Nunca descartar frames necessários para seek/decoder reference, mas na fila já decodificada pode pular frames vencidos para alcançar o clock, mantendo o mais recente <= posição atual.

### 4. Hardware decode opcional
No Windows, avaliar Media Foundation/D3D11VA ou FFmpeg hwaccel como backend opcional. Manter software como fallback. Só vale após benchmark, porque complexidade sobe muito.

### 5. Upload de textura
Já há textura única. Melhorar:
- só atualizar quando frame muda;
- evitar conversão/cópia extra RGBA quando backend permitir buffer reutilizável;
- manter staging buffer reutilizado.

### 6. Buffer adaptativo
BUFFER_FRAMES=60 pode ser excessivo em vídeos pesados e curto em cenários lentos. Melhor usar alvo em milissegundos (ex. 750–1500 ms), limitado por bytes.

### 7. Vsync
O app desabilita vsync globalmente por um problema Wayland. No Windows isso pode aumentar tearing/carga e piorar percepção do vídeo.
Recomendação: tornar a decisão por plataforma:
- Linux/Wayland: manter workaround quando necessário;
- Windows: testar vsync ligado;
- vídeo: não depender de vsync para clock, apenas para apresentação.

## Comparação com upstream 0.15
Não há ganho do upstream que justifique substituir o player de vídeo do fork. O upstream 0.15 foca mais em UI geral e idle media. O ZapExt deve manter sua implementação e importar somente:
- política de não animar mídia fora de foco/hover quando aplicável;
- redução de repaint ocioso;
- testes de consumo idle.

## Benchmarks recomendados
- 10s H.264 480p/720p/1080p;
- 24/30/60 fps;
- CPU média/p95;
- dropped/displayed frames;
- memória resident;
- seek latency p50/p95;
- A/V drift;
- render time por frame.
