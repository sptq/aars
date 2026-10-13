-- Token CSRF per sesja. SameSite=Lax blokuje większość żądań
-- międzywitrynowych, ale spec §8 wymaga tokenu na każdym formularzu
-- zmieniającym stan — stacja, która może wejść na pasmo, nie może polegać
-- na jednej warstwie.
ALTER TABLE sessions ADD COLUMN csrf_token TEXT NOT NULL DEFAULT '';
