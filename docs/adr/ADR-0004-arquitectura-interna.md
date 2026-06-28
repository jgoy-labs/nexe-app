# ADR-0004: Arquitectura interna (thin shell / thick backend)

**Data:** 2026-04-17
**Estat:** Superseded by ADR-0021 (clàusula "NO servir la UI via localhost") — 2026-06-28
**Decidit per:** Jordi Goy

> **Nota (2026-06-28, B175):** la clàusula "NO servir la UI via localhost" (Decisió + taula
> d'alternatives) es va **revertir el 2026-05-21**: la UI la serveix el sidecar via loopback
> (`http://127.0.0.1:{port}/ui/`). Vegeu [ADR-0021](ADR-0021-ui-served-via-loopback.md) per la
> decisió real i les mitigacions. La resta d'aquest ADR (thin shell / thick backend) segueix vigent.

## Context

Cal decidir on viu la lògica de negoci: al Rust core de Tauri o al backend Python existent (server-nexe). Principis a preservar: modularitat, reutilització del que ja funciona, seguretat.

## Decisió

**Thin shell / thick backend:**

- **Tauri (Rust)** = finestra + permisos + lifecycle + IPC natiu (NO lògica de negoci)
- **server-nexe (Python)** = tota la lògica: plugins, RAG, memòria, auth, motors IA
- **Webview principal** = packaged web UI (static assets inside the app)
- **Webview secundari** (opcional) = "Agent Workspace" per preview de l'agent
- **NO servir la UI via localhost** (risc de seguretat documentat per Tauri)

## Alternatives considerades

| Opció | Motiu descart |
|---|---|
| **Rust absorbint lògica de negoci** | Duplicaria server-nexe, violaria modularitat, alt cost dev |
| **UI servida via localhost:8000** | Vulnerabilitat (qualsevol procés local pot accedir), documentada pel propi Tauri |
| **Un sol webview** (sense agent separat) | Impossible mostrar preview navegador real amb webview natiu |
| **Frontend-only Tauri sense server** | Perdríem RAG, memòria, multi-LLM, seguretat ja feta a server-nexe |

## Conseqüències

**Positives:**
- server-nexe existent es reutilitza sencer (zero reescriptura)
- Rust fa allò en què és bo: lifecycle, seguretat, IPC natiu
- Separació neta de capes facilita testing i substitució
- El Rust core NO necessita saber de plugins específics

**Negatives / riscos:**
- IPC híbrid afegeix complexitat (veure ADR-0005)
- Dos runtimes (Rust + Python) = dos sets de crashes potencials
- Ordre d'arrencada important: backend primer, frontend segon

**Mitigacions:**
- Rust intercepta `WindowEvent::CloseRequested` → shutdown HTTP → kill sidecar
- Splash screen "Backend offline/Starting/Ready" amb health check
- Logging unificat a `nexe-app.log`

## Referències

- original plan (not in template)
