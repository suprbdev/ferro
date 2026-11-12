open Lwt.Syntax

module Conf = struct
  let entries = 32
  let stable_hash = 256
  let contents_length_header = Some `Varint
  let inode_child_order = `Seeded_hash
  let forbid_empty_dir_persistence = true
end

module Kv = Irmin_pack_unix.KV (Conf)
module Store = Kv.Make (Irmin.Contents.String)

let keys = 100_000
let batch = 1000
let commits = 1000
let gets = 20_000
let ranges = 1000
let range_len = 100
let merge_commits = 100

let mix x =
  let open Int64 in
  let x = add x 0x9e3779b97f4a7c15L in
  let x = mul (logxor x (shift_right_logical x 30)) 0xbf58476d1ce4e5b9L in
  let x = mul (logxor x (shift_right_logical x 27)) 0x94d049bb133111ebL in
  logxor x (shift_right_logical x 31)

let salted i n = mix (Int64.logxor (Int64.of_int i) (Int64.shift_left 1L n))
let below_keys x = Int64.to_int (Int64.unsigned_rem x (Int64.of_int keys))
let key i = mix (Int64.of_int i)
let value i = Printf.sprintf "value-%d" i
let pick j = below_keys (salted j 50)
let range_start j = Int64.shift_right_logical (salted j 51) 1
let updated i = below_keys (salted i 52)
let buffered i = below_keys (salted i 53)
let hex k = Printf.sprintf "%016Lx" k
let bucket p = Printf.sprintf "%03x" p

let path k =
  let h = hex k in
  [ String.sub h 0 3; h ]

let distinct n f =
  let seen = Hashtbl.create n in
  for i = 0 to n - 1 do
    Hashtbl.replace seen (f i) ()
  done;
  Hashtbl.length seen

let now () = Mtime_clock.now_ns ()
let ms t = Int64.to_float (Int64.sub (now ()) t) /. 1e6

let quantile v q =
  let s = Array.of_list v in
  Array.sort compare s;
  s.(int_of_float (q *. float_of_int (Array.length s - 1)))

let mean v = List.fold_left ( +. ) 0. v /. float_of_int (List.length v)

let rec dir_size p =
  Array.fold_left
    (fun acc name ->
      let p = Filename.concat p name in
      let st = Unix.lstat p in
      match st.Unix.st_kind with
      | Unix.S_DIR -> acc + dir_size p
      | _ -> acc + st.Unix.st_size)
    0 (Sys.readdir p)

let info () = Store.Info.v ~author:"bench" ~message:"c" 0L

let open_repo ~fsync root =
  Store.Repo.v (Irmin_pack.config ~use_fsync:fsync root)

let set t k v = Store.set_exn t ~info (path k) v

let load t lo hi =
  Store.with_tree_exn t [] ~info (fun tree ->
      let tree = Option.value tree ~default:(Store.Tree.empty ()) in
      let rec go tree i =
        if i = hi then Lwt.return (Some tree)
        else
          let* tree = Store.Tree.add tree (path (key i)) (value i) in
          go tree (i + 1)
      in
      go tree lo)

let each n f =
  let rec go j =
    if j = n then Lwt.return_unit
    else
      let* () = f j in
      go (j + 1)
  in
  go 0

let twice f =
  let elapsed = ref 0. in
  let+ () =
    each 2 (fun _ ->
        let t = now () in
        let+ () = f () in
        elapsed := ms t)
  in
  !elapsed

let read_gets tree ~check =
  let rec go j =
    if j = gets then Lwt.return_unit
    else
      let i = pick j in
      let* v = Store.Tree.find tree (path (key i)) in
      match v with
      | Some v when (not check) || v = value i -> go (j + 1)
      | _ -> failwith "get mismatch"
  in
  go 0

let range tree start =
  let lo = hex start in
  let rec take entries got =
    match entries with
    | (_, sub) :: rest when got < range_len -> (
        let* v = Store.Tree.find sub [] in
        match v with
        | Some _ -> take rest (got + 1)
        | None -> failwith "range value missing")
    | _ -> Lwt.return got
  in
  let rec go p got =
    if got >= range_len || p > 0xfff then Lwt.return got
    else
      let* entries = Store.Tree.list tree [ bucket p ] in
      let entries =
        List.filter (fun (s, _) -> String.compare s lo >= 0) entries
        |> List.sort (fun (a, _) (b, _) -> String.compare a b)
      in
      let* got = take entries got in
      go (p + 1) got
  in
  go (Int64.to_int (Int64.shift_right_logical start 52)) 0

let read_ranges tree =
  let rec go j total =
    if j = ranges then
      if total < ranges * range_len * 99 / 100 then failwith "ranges short"
      else Lwt.return_unit
    else
      let* n = range tree (range_start j) in
      go (j + 1) (total + n)
  in
  go 0 0

let tree_at repo h =
  let+ c = Store.Commit.of_hash repo h in
  Store.Commit.tree (Option.get c)

let head_hash t =
  let+ c = Store.Head.get t in
  Store.Commit.hash c

let head_tree t =
  let+ c = Store.Head.get t in
  Store.Commit.tree c

let main dir =
  let root = Filename.concat dir "db" in
  let out = Hashtbl.create 32 in
  let put k v = Hashtbl.replace out k v in

  let* repo = open_repo ~fsync:true root in
  let* t = Store.main repo in
  let lat = ref [] in
  let start = now () in
  let* () =
    each (keys / batch) (fun c ->
        let s = now () in
        let+ () = load t (c * batch) ((c + 1) * batch) in
        lat := ms s :: !lat)
  in
  put "load_s" (ms start /. 1e3);
  put "batch_ms" (quantile !lat 0.5);
  let* v0 = head_hash t in
  let* () = Store.Repo.close repo in
  let size_load = dir_size root in
  put "size_load_mib" (float_of_int size_load /. 1048576.);

  let s = now () in
  let* repo = open_repo ~fsync:true root in
  let* t = Store.main repo in
  let* v = Store.find t (path (key 0)) in
  assert (v <> None);
  put "reopen_ms" (ms s);

  lat := [];
  let* () =
    each commits (fun i ->
        let s = now () in
        let+ () = set t (key (updated i)) (Printf.sprintf "upd-%d" i) in
        lat := ms s :: !lat)
  in
  let* v1 = head_hash t in
  put "commit_p50_ms" (quantile !lat 0.5);
  put "commit_p99_ms" (quantile !lat 0.99);
  put "commit_mean_ms" (mean !lat);
  let* () = Store.Repo.close repo in
  put "commit_bytes"
    (float_of_int (dir_size root - size_load) /. float_of_int commits);

  let* repo = open_repo ~fsync:false root in
  let* t = Store.main repo in
  lat := [];
  let* () =
    each commits (fun i ->
        let s = now () in
        let+ () = set t (key (buffered i)) (Printf.sprintf "buf-%d" i) in
        lat := ms s :: !lat)
  in
  put "buffered_p50_ms" (quantile !lat 0.5);
  put "buffered_mean_ms" (mean !lat);
  let* () = Store.Repo.close repo in

  let* repo = open_repo ~fsync:true root in
  let* t = Store.main repo in
  let* e =
    twice (fun () ->
        let* tree = head_tree t in
        read_gets tree ~check:false)
  in
  put "get_us" (e *. 1e3 /. float_of_int gets);
  let* e =
    twice (fun () ->
        let* tree = head_tree t in
        read_ranges tree)
  in
  put "range_us" (e *. 1e3 /. float_of_int ranges);
  let* e =
    twice (fun () ->
        let* tree = tree_at repo v0 in
        read_gets tree ~check:true)
  in
  put "old_get_us" (e *. 1e3 /. float_of_int gets);
  let* e =
    twice (fun () ->
        let* tree = tree_at repo v0 in
        read_ranges tree)
  in
  put "old_range_us" (e *. 1e3 /. float_of_int ranges);

  let s = now () in
  let* before = tree_at repo v0 in
  let* after = tree_at repo v1 in
  let* changes = Store.Tree.diff before after in
  put "diff_ms" (ms s);
  let changes = List.length changes in
  if changes <> distinct commits updated then
    failwith (Printf.sprintf "diff found %d changes" changes);
  put "diff_changes" (float_of_int changes);

  let s = now () in
  let* checked = Store.integrity_check ~auto_repair:false repo in
  put "verify_ms" (ms s);
  (match checked with
  | Ok `No_error -> ()
  | Ok (`Fixed n) -> failwith (Printf.sprintf "verify fixed %d" n)
  | Error (`Corrupted n) -> failwith (Printf.sprintf "verify found %d" n)
  | Error (`Cannot_fix m) -> failwith m);
  let* () = Store.Repo.close repo in

  let* repo = open_repo ~fsync:true root in
  let* t = Store.main repo in
  let* v = Store.find t (path (key 0)) in
  assert (v <> None);
  let s = now () in
  let* () = set t (key (keys - 1)) "first" in
  put "first_commit_ms" (ms s);
  let s = now () in
  let* b = Store.clone ~src:t ~dst:"b" in
  put "fork_ms" (ms s);
  let* () =
    each merge_commits (fun j -> set b (key (2 * j)) (Printf.sprintf "br-%d" j))
  in
  let* () =
    each merge_commits (fun j ->
        set t (key ((2 * j) + 1)) (Printf.sprintf "mn-%d" j))
  in
  let s = now () in
  let* merged = Store.merge_with_branch t ~info "b" in
  put "merge_ms" (ms s);
  (match merged with
  | Ok () -> ()
  | Error (`Conflict m) -> failwith ("merge conflict: " ^ m));
  let* a = Store.get t (path (key 0)) in
  let* c = Store.get t (path (key 1)) in
  if a <> "br-0" || c <> "mn-0" then failwith "merge lost a side";
  let* head = Store.Head.get t in
  if List.length (Store.Commit.parents head) <> 2 then
    failwith "merge commit does not have two parents";

  let* () = Store.Repo.close repo in
  put "size_end_mib" (float_of_int (dir_size root) /. 1048576.);

  let names = Hashtbl.fold (fun k _ acc -> k :: acc) out [] in
  let parts =
    List.sort String.compare names
    |> List.map (fun k -> Printf.sprintf "%s=%.9g" k (Hashtbl.find out k))
  in
  print_endline ("RESULT " ^ String.concat " " parts);
  Lwt.return_unit

let () =
  match Sys.argv with
  | [| _; dir |] -> Lwt_main.run (main dir)
  | _ ->
      prerr_endline "usage: irmin <dir>";
      exit 2
