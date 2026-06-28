# ADR-0021: UI servida pel sidecar via loopback HTTP

**Data:** 2026-06-28
**Estat:** Accepted
**Decidit per:** Jordi Goy
**Supersedeix:** ADR-0004 (la clàusula "NO servir la UI via localhost")

## Context

L'ADR-0004 (2026-04-17) va decidir que el webview principal carregaria **packaged
static assets** dins l'app i va prohibir explícitament "servir la UI via localhost"
per risc de seguretat. La implementació real va divergir: el **revert del 2026-05-21**
(`src-tauri/src/lib.rs`) navega el webview a `http://127.0.0.1:{port}/ui/#nexe_api_key=…`,
és a dir la UI la **serveix el sidecar Python** (server-nexe `web_ui_module`, `routes.py`)
per loopback. Aquesta decisió mai es va documentar ni es va actualitzar l'ADR-0004, que
seguia marcat `Accepted` contradient el codi enviat a v1.0.6 (finding B175, docs-honesty).

Aquest ADR ratifica la decisió real i en documenta les mitigacions i el risc residual.

## Decisió

**La UI es serveix pel sidecar via loopback HTTP** (no com a packaged assets):

- El webview navega a `http://127.0.0.1:{port}/ui/` amb el port **efímer dinàmic**.
- L'`nexe_api_key` viatja al **fragment** de la URL (`#nexe_api_key=…`), que el navegador
  **mai envia al servidor**; el frontend l'extreu i tot seguit fa `history.replaceState`
  per netejar-lo de l'historial.
- Motiu del revert: servir la UI des del sidecar evita duplicar/empaquetar els assets i
  manté una sola font de veritat de la UI (la de server-nexe).

## Mitigacions del risc loopback

| Risc | Mitigació | Evidència |
|---|---|---|
| Qualsevol procés local podria connectar | Bind **només a `127.0.0.1`** | `lib.rs` `NEXE_HOST=127.0.0.1` |
| Port previsible | **Port efímer** reservat en runtime | `reserve_ephemeral_port` (`lib.rs`) |
| Fuita de l'API key | key al **fragment** (mai enviat al servidor) + `replaceState` scrub | `lib.rs` (navegació + scrub) |
| Accés sense credencial | Tots els endpoints exigeixen **Bearer/X-API-Key** | ADR-0008 (Zero Trust) |

**Risc residual honest:** un procés local maliciós que ja conegués el port efímer i la key
podria parlar amb el sidecar; però en el model single-user local-first això equival al nivell
de confiança de la pròpia màquina. No és el packaged-assets que ADR-0004 preferia, però el
risc està acotat i acceptat.

## Conseqüències

- ADR-0004 queda **Superseded** per aquest ADR (la resta de l'ADR-0004 —thin shell/thick
  backend— segueix vigent; només es revoca la clàusula "no UI via localhost").
- Si en el futur es vol tornar a packaged assets / `tauri://localhost`, caldrà un build step
  i un ADR nou (canvi gran d'arquitectura, fora d'aquest registre).

## Referències

- [ADR-0004](ADR-0004-arquitectura-interna.md) (superseded clause)
- [ADR-0008](ADR-0008-seguretat-zero-trust.md) (Zero Trust local)
- `src-tauri/src/lib.rs` (revert 2026-05-21: navegació loopback + scrub del fragment)
