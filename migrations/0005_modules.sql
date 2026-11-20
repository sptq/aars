-- Rejestr modułów stacji (etap 9): harmonogram jest przełączalny i domyślnie
-- włączony. Identyfikacja jest zawsze włączona, więc nie ma wiersza; biuletyn
-- jest zapowiedziany (etap 11).
INSERT INTO settings (key, value) VALUES ('module:scheduler:enabled', 'true');
