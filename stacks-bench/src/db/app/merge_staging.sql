-- Merge transaction Types
INSERT INTO
    stacks_tx_type (name)
SELECT DISTINCT
    name
FROM _staged_stacks_tx_type
WHERE
    true
ON CONFLICT (name) DO NOTHING;

-- Merge principals
INSERT INTO
    principal (address)
SELECT DISTINCT
    address
FROM _staged_principal
WHERE
    true
ON CONFLICT (address) DO NOTHING;

-- Merge contracts
INSERT INTO
    contract (issuer_principal_id, name)
SELECT p.id, sc.name
FROM
    _staged_contract sc
    JOIN principal p ON sc.issuer_address = p.address
WHERE
    true
ON CONFLICT (issuer_principal_id, name) DO NOTHING;

-- Merge Burn blocks
INSERT INTO
    burn_block (block_hash, height)
SELECT DISTINCT
    burn_block_hash,
    burn_block_height
FROM _staged_stacks_block
WHERE
    true
ON CONFLICT (block_hash) DO NOTHING;

-- Merge Stacks blocks (initial insert with NULL parent)
INSERT INTO
    stacks_block (
        index_hash,
        block_hash,
        height,
        burn_block_id,
        parent_stacks_block_id
    )
SELECT sb.index_hash, sb.block_hash, sb.height, bb.id, NULL
FROM
    _staged_stacks_block sb
    JOIN burn_block bb ON sb.burn_block_hash = bb.block_hash
WHERE
    true
ON CONFLICT (index_hash) DO NOTHING;

-- Link Stacks block parents (SQLite UPDATE FROM syntax)
UPDATE stacks_block
SET
    parent_stacks_block_id = parent.id
FROM
    _staged_stacks_block stage,
    stacks_block parent
WHERE
    stacks_block.index_hash = stage.index_hash
    AND parent.index_hash = stage.parent_index_hash;

-- Merge Transactions
-- Resolves tx_type, caller_principal, and contract_id.
INSERT INTO
    stacks_tx (
        stacks_block_id,
        tx_hash,
        stacks_tx_type_id,
        caller_principal_id,
        contract_id
    )
SELECT b.id, st.tx_hash, tt.id, p_caller.id, c.id
FROM
    _staged_stacks_tx st
    JOIN stacks_block b ON st.block_index_hash = b.index_hash
    JOIN stacks_tx_type tt ON st.tx_type = tt.name -- Resolve Caller
    LEFT JOIN principal p_caller ON st.caller_address = p_caller.address -- Resolve Contract (requires joining principal first to get the owner ID)
    LEFT JOIN principal p_contract_issuer ON st.contract_issuer_address = p_contract_issuer.address
    LEFT JOIN contract c ON c.issuer_principal_id = p_contract_issuer.id AND c.name = st.contract_name
WHERE
    true
ON CONFLICT (stacks_block_id, tx_hash) DO NOTHING;