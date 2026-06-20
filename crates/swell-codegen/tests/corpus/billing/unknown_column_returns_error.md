# Unknown column returns a clear error

`analyze` must surface Postgres's PARSE error with the offending
column name in the message.

```sql
SELECT no_such_column FROM billing.users
```

```err
no_such_column
```
