import type { QueryResult, QueryResultRow } from "pg";
import type { SqlText } from "./index";
declare module "pg" {
    interface ClientBase {
        query<P extends unknown[], R extends QueryResultRow>(queryText: SqlText<P, R>, values?: P): Promise<QueryResult<R>>;
    }
    interface Pool {
        query<P extends unknown[], R extends QueryResultRow>(queryText: SqlText<P, R>, values?: P): Promise<QueryResult<R>>;
    }
}
//# sourceMappingURL=pg.d.ts.map