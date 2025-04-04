use crate::errors::MarginError;
use crate::instructions::ExecuteWithdrawal;
use anchor_lang::prelude::*;
use chainlink_solana as chainlink;
use perp_amm::cpi::{admin_withdraw, direct_deposit};

/**
 * @dev Helper function to process PnL updates.
 * Essentially calculates the available margin in USD, then allocates the PnL proportionally
 * between SOL and USDC.
 * FIXME: What if pool has no USDC, only SOL? --> maybe better to choose a currency to settle pnl in.
 * FIXME: Uses SOL / USD price, but we're not actually updating the price feed.
 */
pub fn process_pnl_update(ctx: &mut Context<ExecuteWithdrawal>, pnl_update: i64) -> Result<()> {
    let pool_state = &mut ctx.accounts.pool_state;

    // Update SOL/USD price from Chainlink before using it
    let round = chainlink::latest_round_data(
        ctx.accounts.chainlink_program.to_account_info(),
        ctx.accounts.chainlink_feed.to_account_info(),
    )?;
    let sol_usd_price: i128 = round.answer;

    // Determine which asset to use for settlement based on the provided pool_vault_account
    let use_sol_for_settlement = ctx.accounts.pool_vault_account.key() == pool_state.sol_vault;

    // Skip processing if PnL is zero
    if pnl_update == 0 {
        return Ok(());
    }

    // Convert pnl_update to a u128 and upscale from 6 to 8 decimals
    let pnl_total_usd = pnl_update.unsigned_abs() as u128;
    let pnl_total_adj = pnl_total_usd
        .checked_mul(100)
        .ok_or(MarginError::ArithmeticOverflow)?;

    if pnl_update > 0 {
        process_positive_pnl(ctx, pnl_total_adj, use_sol_for_settlement, sol_usd_price)?;
    } else {
        process_negative_pnl(ctx, pnl_total_adj, use_sol_for_settlement, sol_usd_price)?;
    }

    Ok(())
}

// Helper function to process positive PnL
fn process_positive_pnl(
    ctx: &mut Context<ExecuteWithdrawal>,
    pnl_total_usd: u128,
    use_sol_for_settlement: bool,
    sol_usd_price: i128,
) -> Result<()> {
    let margin_account = &mut ctx.accounts.margin_account;

    if use_sol_for_settlement {
        // Convert PnL from USD (8 decimals) to SOL (9 decimals)
        let pnl_sol_native = pnl_total_usd
            .checked_mul(1_000_000_000)
            .ok_or(MarginError::ArithmeticOverflow)?
            .checked_div(sol_usd_price as u128)
            .ok_or(MarginError::ArithmeticOverflow)? as u64;

        if pnl_sol_native > 0 {
            let cpi_program = ctx.accounts.liquidity_pool_program.to_account_info();

            let cpi_accounts = perp_amm::cpi::accounts::AdminWithdraw {
                admin: ctx.accounts.authority.to_account_info(),
                pool_state: ctx.accounts.pool_state.to_account_info(),
                vault_account: ctx.accounts.pool_vault_account.to_account_info(),
                admin_token_account: ctx.accounts.margin_sol_vault.to_account_info(),
                chainlink_program: ctx.accounts.chainlink_program.to_account_info(),
                chainlink_feed: ctx.accounts.chainlink_feed.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);

            admin_withdraw(cpi_ctx, pnl_sol_native)?;

            margin_account.sol_balance = margin_account
                .sol_balance
                .checked_add(pnl_sol_native)
                .ok_or(MarginError::ArithmeticOverflow)?;
        }
    } else {
        // Using USDC for settlement
        // Convert PnL from USD (8 decimals) to USDC (6 decimals)
        let pnl_usdc_native = (pnl_total_usd
            .checked_div(100)
            .ok_or(MarginError::ArithmeticOverflow)?) as u64;

        if pnl_usdc_native > 0 {
            let cpi_program = ctx.accounts.liquidity_pool_program.to_account_info();
            let cpi_accounts = perp_amm::cpi::accounts::AdminWithdraw {
                admin: ctx.accounts.authority.to_account_info(),
                pool_state: ctx.accounts.pool_state.to_account_info(),
                vault_account: ctx.accounts.pool_vault_account.to_account_info(),
                admin_token_account: ctx.accounts.margin_usdc_vault.to_account_info(),
                chainlink_program: ctx.accounts.chainlink_program.to_account_info(),
                chainlink_feed: ctx.accounts.chainlink_feed.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);

            admin_withdraw(cpi_ctx, pnl_usdc_native)?;

            margin_account.usdc_balance = margin_account
                .usdc_balance
                .checked_add(pnl_usdc_native)
                .ok_or(MarginError::ArithmeticOverflow)?;
        }
    }

    Ok(())
}

// Helper function to process negative PnL
fn process_negative_pnl(
    ctx: &mut Context<ExecuteWithdrawal>,
    pnl_total_usd: u128,
    use_sol_for_settlement: bool,
    sol_usd_price: i128,
) -> Result<()> {
    let margin_account = &mut ctx.accounts.margin_account;

    if use_sol_for_settlement {
        // Convert PnL from USD (8 decimals) to SOL (9 decimals)
        let pnl_sol_native = pnl_total_usd
            .checked_mul(1_000_000_000)
            .ok_or(MarginError::ArithmeticOverflow)?
            .checked_div(sol_usd_price as u128)
            .ok_or(MarginError::ArithmeticOverflow)? as u64;

        // Limit deduction to available balance
        let deduct_sol = std::cmp::min(pnl_sol_native, margin_account.sol_balance);

        if deduct_sol > 0 {
            // Update margin account balance
            margin_account.sol_balance = margin_account
                .sol_balance
                .checked_sub(deduct_sol)
                .ok_or(MarginError::ArithmeticOverflow)?;

            // First, transfer tokens from margin_sol_vault to authority_token_account
            let seeds = &[b"margin_vault".as_ref(), &[ctx.accounts.margin_vault.bump]];
            let signer_seeds = &[&seeds[..]];
            
            let transfer_accounts = anchor_spl::token::Transfer {
                from: ctx.accounts.margin_sol_vault.to_account_info(),
                to: ctx.accounts.authority_token_account.to_account_info(),
                authority: ctx.accounts.margin_vault.to_account_info(),
            };
            
            let transfer_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                transfer_accounts,
                signer_seeds,
            );
            
            // Transfer the tokens to the authority's account first
            anchor_spl::token::transfer(transfer_ctx, deduct_sol)?;
            
            // Now have the authority make the direct deposit
            let cpi_program = ctx.accounts.liquidity_pool_program.to_account_info();
            let cpi_accounts = perp_amm::cpi::accounts::DirectDeposit {
                // Use the authority as the depositor (who is a real signer)
                depositor: ctx.accounts.authority.to_account_info(),
                pool_state: ctx.accounts.pool_state.to_account_info(),
                // Use the authority's token account as the source
                depositor_token_account: ctx.accounts.authority_token_account.to_account_info(),
                vault_account: ctx.accounts.pool_vault_account.to_account_info(),
                chainlink_program: ctx.accounts.chainlink_program.to_account_info(),
                chainlink_feed: ctx.accounts.chainlink_feed.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            
            // No need for PDA signing here - authority is a real signer
            let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
            direct_deposit(cpi_ctx, deduct_sol)?;
        }
    } else {
        // Using USDC for settlement
        // Convert PnL from USD (8 decimals) to USDC (6 decimals)
        let pnl_usdc_native = (pnl_total_usd
            .checked_div(100)
            .ok_or(MarginError::ArithmeticOverflow)?) as u64;

        // Limit deduction to available balance
        let deduct_usdc = std::cmp::min(pnl_usdc_native, margin_account.usdc_balance);

        if deduct_usdc > 0 {
            // Update margin account balance
            margin_account.usdc_balance = margin_account
                .usdc_balance
                .checked_sub(deduct_usdc)
                .ok_or(MarginError::ArithmeticOverflow)?;

            // First, transfer tokens from margin_usdc_vault to authority_token_account
            let seeds = &[b"margin_vault".as_ref(), &[ctx.accounts.margin_vault.bump]];
            let signer_seeds = &[&seeds[..]];
            
            let transfer_accounts = anchor_spl::token::Transfer {
                from: ctx.accounts.margin_usdc_vault.to_account_info(),
                to: ctx.accounts.authority_token_account.to_account_info(),
                authority: ctx.accounts.margin_vault.to_account_info(),
            };
            
            let transfer_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                transfer_accounts,
                signer_seeds,
            );
            
            // Transfer the tokens to the authority's account first
            anchor_spl::token::transfer(transfer_ctx, deduct_usdc)?;
            
            // Now have the authority make the direct deposit
            let cpi_program = ctx.accounts.liquidity_pool_program.to_account_info();
            let cpi_accounts = perp_amm::cpi::accounts::DirectDeposit {
                // Use the authority as the depositor (who is a real signer)
                depositor: ctx.accounts.authority.to_account_info(),
                pool_state: ctx.accounts.pool_state.to_account_info(),
                // Use the authority's token account as the source
                depositor_token_account: ctx.accounts.authority_token_account.to_account_info(),
                vault_account: ctx.accounts.pool_vault_account.to_account_info(),
                chainlink_program: ctx.accounts.chainlink_program.to_account_info(),
                chainlink_feed: ctx.accounts.chainlink_feed.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            
            // No need for PDA signing here - authority is a real signer
            let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
            direct_deposit(cpi_ctx, deduct_usdc)?;
        }
    }

    Ok(())
}
