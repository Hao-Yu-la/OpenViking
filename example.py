import tempfile, os
import time
import openviking as ov

# Initialize OpenViking client with data directory
client = ov.OpenViking(path="./data")

try:
    # Initialize the client
    client.initialize()

    # Add resource (supports URL, file, or directory)
    # Local directory scans respect .gitignore by default.
    # Wait until semantic processing completes before inspecting the resource.
    print("Wait for semantic processing...")
    notes_path = os.path.join(tempfile.mkdtemp(), "notes_1.md")
    with open(notes_path, "w") as f:
        f.write("# Notes\nA first document.\n")
    add_result = client.add_resource(path=notes_path, wait=True)
    root_uri = add_result['root_uri']

    # Explore the resource tree structure
    ls_result = client.ls(root_uri)
    print(f"Directory structure:\n{ls_result}\n")

    # Use glob to find markdown files
    glob_result = client.glob(pattern="**/*.md", uri=root_uri)
    if glob_result['matches']:
        content = client.read(glob_result['matches'][0])
        print(f"Content preview: {content[:200]}...\n")

    # Get abstract and overview of the resource
    abstract = client.abstract(root_uri)
    overview = client.overview(root_uri)
    print(f"Abstract:\n{abstract}\n\nOverview:\n{overview}\n")

    # Perform semantic search
    results = client.find("just test")
    print("Search results:")
    for r in results.resources:
        print(f"  {r.uri} (score: {r.score:.4f})")

    # ------------------------------------------------------------------
    # Multi-version management with client.git.*
    #
    # Requires git to be enabled in ov.conf:
    # "git": {                                                                                                                                 
    #     "enabled": true,                                                                                                                     
    #     "backend": "local",                               
    #     "default_branch": "main",
    #     "author_name": "viking-bot",                                                                                                         
    #     "author_email": "bot@viking.local",                                                                                                  
    #     "local": {                                                                                                                           
    #         "base_dir": "",           # defaults to {storage_path}/git                                                                                                       
    #         "fsync": "off"                                                                                                                   
    #     }                                                 
    # }    
    # ------------------------------------------------------------------
    print("\n=== Version control demo ===")

    # Snapshot v1: the initial README we just added
    v1 = client.git.commit(message="snapshot v1: initial README")
    print(f"v1 commit: {v1['commit_oid'][:12]}  ({v1['result']})")

    # Add a second resource and snapshot v2
    notes_path = os.path.join(tempfile.mkdtemp(), "notes_2.md")
    with open(notes_path, "w") as f:
        f.write("# Notes\nA second document.\n")
    print("\nWait for semantic processing...")
    client.add_resource(path=notes_path, wait=True)
    v2 = client.git.commit(message="snapshot v2: add notes")
    print(f"v2 commit: {v2['commit_oid'][:12]}  ({v2['result']})")

    # Walk the history (newest first)
    print("\nHistory (newest first):")
    for entry in client.git.log(limit=10):
        print(f"  {entry['oid'][:12]}  {entry['message']}")

    # Perform semantic search
    time.sleep(3)
    results = client.find("just test")
    print("\nSearch results:")
    for r in results.resources:
        print(f"  {r.uri} (score: {r.score:.4f})")

    # Read a specific blob at v1: notes.md should not exist there yet
    readme_uri = glob_result['matches'][0] if glob_result['matches'] else None
    if readme_uri:
        blob_at_v1 = client.git.show(v1['commit_oid'], path=readme_uri)
        print(f"\nREADME at v1: {len(blob_at_v1)} bytes")

    # Roll the resources subtree back to v1.
    # restore() advances HEAD with a new commit whose content matches v1 — it
    # does not move the branch ref backwards, so history stays append-only.
    print("\nRolling resources/ back to v1...")
    rollback = client.git.restore(
        project_dir="viking://resources",
        source_commit=v1['commit_oid'],
    )
    written_paths = rollback.get('written_paths', [])
    deleted_paths = rollback.get('deleted_paths', [])

    print(f"  result:        {rollback['result']}")
    print(f"  source:        {rollback['source_commit'][:12]}")
    print(f"  parent:        {rollback['parent_commit'][:12]}")
    print(f"  new_commit:    {rollback['new_commit_oid'][:12]}")
    print(f"  written_paths: {len(written_paths)} file(s)")
    for path in written_paths:
        print(f"    + {path}")
    print(f"  deleted_paths: {len(deleted_paths)} file(s)")
    for path in deleted_paths:
        print(f"    - {path}")

    # Re-run the semantic search: notes_2.md is gone, find() reflects v1's view
    # once the background reindex finishes.
    client.wait_processed()
    results_after = client.find("just test")
    print("\nSearch results after rollback:")
    for r in results_after.resources:
        print(f"  {r.uri} (score: {r.score:.4f})")

    # Preview a rollback without mutating anything
    dry = client.git.restore(
        project_dir="viking://resources",
        source_commit=v2['commit_oid'],
        dry_run=True,
    )
    diff = dry.get('diff', {})
    to_write = diff.get('to_write', [])
    to_delete = diff.get('to_delete', [])
    unchanged = diff.get('unchanged', [])

    print("\nDry-run to v2:")
    print(f"  result:    {dry.get('result')}")
    print(f"  head:      {dry.get('head', '')[:12]}")
    print(f"  source:    {dry.get('source', '')[:12]}")
    print(f"  to_write:  {len(to_write)} file(s)")
    for item in to_write:
        print(f"    + {item['path']}  ({item['oid'][:12]})")
    print(f"  to_delete: {len(to_delete)} file(s)")
    for item in to_delete:
        if isinstance(item, dict):
            print(f"    - {item.get('path', '<unknown>')}")
        else:
            print(f"    - {item}")
    print(f"  unchanged: {len(unchanged)} file(s)")

    # Close the client
    client.close()

except Exception as e:
    print(f"Error: {e}")
