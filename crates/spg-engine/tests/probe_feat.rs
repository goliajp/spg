use spg_engine::{Engine, QueryResult};
fn run(e:&mut Engine, sql:&str)->String{
  match e.execute(sql){ Ok(QueryResult::Rows{rows,..})=>format!("{:?}",rows[0].values[0]), Ok(_)=>"OK".into(), Err(x)=>format!("ERR {}",format!("{x:?}").chars().take(50).collect::<String>()) }
}
#[test]
fn probe(){
  let mut e=Engine::new();
  for (t,s) in [
    ("jsonb literal obj","SELECT '{\"a\":1,\"b\":2}'::jsonb"),
    ("jsonb literal arr","SELECT '[1,2,3]'::jsonb"),
    ("to_jsonb arr","SELECT to_jsonb(ARRAY[1,2])"),
    ("jsonb_agg","SELECT jsonb_agg(x) FROM (SELECT 1 x UNION ALL SELECT 2) s"),
    ("jsonb_build_obj","SELECT jsonb_build_object('a',1,'b',2)"),
    ("json literal(text)","SELECT '{\"a\":1,\"b\":2}'::json"),
    ("jsonb nested","SELECT '{\"a\":[1,2],\"b\":{\"c\":3}}'::jsonb"),
  ]{ println!("[{t}] {}", run(&mut e,s)); }
}
