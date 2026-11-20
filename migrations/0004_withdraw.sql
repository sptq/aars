-- Wycofanie komunikatu, który jest właśnie wzięty z kolejki (etap 8, I1).
-- `transmitting` znaczy też „wzięty, renderowany, zaraz odrzucony przez
-- interlock”. Takie anulowanie zapamiętujemy: gdy element wraca przez
-- hold/release_claim, kończy jako `cancelled`, zamiast znów czekać.
ALTER TABLE queue_items ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0;

-- Wcześniejsze elementy kolejki pozycji źródła, zastąpione zmianą, ale
-- jeszcze wzięte do nadania: jeśli któryś skończy jako `done`, pozycja
-- poszła na antenę. Lista identyfikatorów rozdzielonych przecinkami.
ALTER TABLE source_items ADD COLUMN superseded_queue_ids TEXT NOT NULL DEFAULT '';
-- Odwołanie czeka, aż wyjaśni się, czy oryginał poszedł na antenę.
ALTER TABLE source_items ADD COLUMN cancel_pending INTEGER NOT NULL DEFAULT 0;
