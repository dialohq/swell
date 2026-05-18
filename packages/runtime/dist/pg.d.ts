import type { Submittable, QueryArrayConfig, QueryArrayResult, QueryConfig, QueryConfigValues, QueryResult, QueryResultRow } from "pg";
import type { SqlText } from "./index";
declare module "pg" {
    interface ClientBase {
        query<P extends unknown[], R>(queryText: SqlText<P, R>, values?: P): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
    }
    interface Pool {
        query<P extends unknown[], R>(queryText: SqlText<P, R>, values?: P): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
    }
}
export type RawSql = string & {
    readonly __sqlBrand?: never;
};
export type QueryType = {
    <P extends unknown[], R>(queryText: SqlText<P, R>, values?: P): Promise<QueryResult<R extends QueryResultRow ? R : QueryResultRow>>;
    <T extends Submittable>(queryStream: T): Promise<T>;
    <R extends any[] = any[], I = any[]>(queryConfig: QueryArrayConfig<I>, values?: QueryConfigValues<I>): Promise<QueryArrayResult<R>>;
    <R extends QueryResultRow = any, I = any>(queryConfig: QueryConfig<I>): Promise<QueryResult<R>>;
    <R extends QueryResultRow = any, I = any[]>(queryTextOrConfig: RawSql | QueryConfig<I>, values?: QueryConfigValues<I>): Promise<QueryResult<R>>;
};
//# sourceMappingURL=pg.d.ts.map