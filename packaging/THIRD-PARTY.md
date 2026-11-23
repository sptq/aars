# Oprogramowanie innych autorów w obrazie AARS

Obraz Raspberry Pi z AARS zawiera, poza samym Raspberry Pi OS Lite (licencje
pakietów Debiana leżą w `/usr/share/doc/*/copyright`), następujące składniki:

| Składnik | Wersja | Licencja | Skąd |
|---|---|---|---|
| AARS | zgodnie z `/opt/aars/current/VERSION` | MIT | https://github.com/sptq/aars |
| piper-tts (OHF-Voice/piper1-gpl) | 1.8.0 | GPL-3.0 | https://github.com/OHF-Voice/piper1-gpl |
| espeak-ng (w kole piper-tts) | dołączony do piper-tts 1.8.0 | GPL-3.0 | https://github.com/espeak-ng/espeak-ng |
| onnxruntime | 1.32.0 | MIT | https://github.com/microsoft/onnxruntime |
| numpy | 2.5.5 | BSD-3-Clause | https://numpy.org |
| protobuf | 7.36.2 | BSD-3-Clause | https://github.com/protocolbuffers/protobuf |
| flatbuffers | 25.12.19 | Apache-2.0 | https://github.com/google/flatbuffers |
| packaging | 26.3 | Apache-2.0 lub BSD-2-Clause | https://github.com/pypa/packaging |
| pathvalidate | 3.3.1 | MIT | https://github.com/thombashi/pathvalidate |
| Głos `pl_PL-darkman-medium` | rewizja `e9c8a39444359388adaa94255becb37a5a2b8e51` | zbiór danych CC0 | https://huggingface.co/rhasspy/piper-voices |

Piper działa w osobnym venv (`/opt/aars/piper`) i AARS uruchamia go jako
osobny proces, więc licencja GPL obejmuje Pipera, a nie kod AARS.

Źródło piper-tts 1.8.0 (`piper_tts-1.8.0.tar.gz`, SHA-256
`830588aded347df579c91a32703e0fc2a3685d84f1e3533b14f2de69135d4904`) jest
dołączone do każdego wydania AARS na GitHubie, obok obrazu.
