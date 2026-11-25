-- Etap 13: tempo 0,87 okazało się za wolne; domyślnie ok. 7% wolniej niż model.
-- Zmienia tylko niezmienioną wartość domyślną, wybór operatora zostaje.
UPDATE settings SET value = '0.93' WHERE key = 'speech_rate' AND value = '0.87';
