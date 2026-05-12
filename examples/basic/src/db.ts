import postgres from "postgres";
import { createSql } from "./swell.generated";

export const sql = createSql(postgres());
