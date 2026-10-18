-- Row engine scalar functions vs sqlite3 3.53.4 (#651): one statement
-- per line, run by tools/check_row_functions_oracle.py.
-- harness self-check: functions db-core already had
SELECT upper('abc'), lower('ÀBC'), length('héllo'), abs(-3), abs(-2.5)
SELECT round(2.5), round(-2.5), round(1.2345, 2), typeof(round(1))
SELECT substr('hello', 2, 3), replace('aXbX', 'X', '-'), instr('hello', 'l')
SELECT hex('ab'), quote('it''s'), coalesce(NULL, 2), nullif(1, 1), iif(0, 'a', 'b')
SELECT 1.0 / 3, 1e300 * 10, -0.0, 0.1 + 0.2
SELECT nosuch(1)
