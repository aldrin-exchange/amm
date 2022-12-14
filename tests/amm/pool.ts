import {
  createAccount,
  createMint,
  getAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import {
  AccountMeta,
  PublicKey,
  Keypair,
  Signer,
  SystemProgram,
} from "@solana/web3.js";
import { airdrop, amm, payer, provider } from "../helpers";
import { createProgramToll, discountAddress, programTollAddress } from "./amm";
import { BN } from "@project-serum/anchor";

export interface DepositLiquidityArgs {
  user: Keypair;
  pool: PublicKey;
  poolSignerPda: PublicKey;
  lpMint: PublicKey;
  lpTokenWallet: PublicKey;
  maxAmountTokens: { mint: PublicKey; tokens: { amount: BN } }[];
  vaultsAndWallets: AccountMeta[];
}

export interface RedeemLiquidityArgs {
  user: Keypair;
  pool: PublicKey;
  poolSigner: PublicKey;
  lpMint: PublicKey;
  lpTokenWallet: PublicKey;
  minAmountTokens: { mint: PublicKey; tokens: { amount: BN } }[];
  lpTokensToBurn: number;
  vaultsAndWallets: AccountMeta[];
}

export class Pool {
  private constructor(public id: Keypair, public admin: Keypair) {
    //
  }

  public static async init(amplifier = 0): Promise<Pool> {
    const id = Keypair.generate();

    const admin = Keypair.generate();
    await airdrop(admin.publicKey);

    const toll = await programTollAddress();
    let tollAuthority = payer.publicKey;
    try {
      const info = await amm.account.programToll.fetch(toll);
      tollAuthority = info.authority;
    } catch {
      await createProgramToll(tollAuthority);
    }

    const poolSigner = Pool.signerFrom(id.publicKey);

    const lpMint = await createMint(
      provider.connection,
      payer,
      poolSigner,
      null,
      9
    );
    const programTollWallet = await createAccount(
      provider.connection,
      payer,
      lpMint,
      tollAuthority
    );

    const vaults = await Promise.all(
      new Array(2).fill(undefined).map(async () => {
        const mint = await createMint(
          provider.connection,
          payer,
          id.publicKey,
          null,
          9
        );
        const kp = Keypair.generate();
        await createAccount(provider.connection, payer, mint, poolSigner, kp);
        return {
          isSigner: false,
          isWritable: false,
          pubkey: kp.publicKey,
        };
      })
    );

    await amm.methods
      .createPool(new BN(amplifier))
      .accounts({
        admin: admin.publicKey,
        pool: id.publicKey,
        programToll: toll,
        poolSigner,
        programTollWallet,
        lpMint,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .remainingAccounts(vaults)
      .signers([admin, id])
      .rpc();

    return new Pool(id, admin);
  }

  public async fetch() {
    return amm.account.pool.fetch(this.id.publicKey);
  }

  public static signerFrom(publicKey: PublicKey): PublicKey {
    return PublicKey.findProgramAddressSync(
      [Buffer.from("signer"), publicKey.toBytes()],
      amm.programId
    )[0];
  }

  public signer(): PublicKey {
    return Pool.signerFrom(this.id.publicKey);
  }

  public signerPda(): PublicKey {
    return Pool.signerFrom(this.id.publicKey);
  }

  public static async airdropLiquidityTokens(
    mint: PublicKey,
    wallet: PublicKey,
    authority: Signer,
    amount: number = 1_000_000
  ) {
    return mintTo(provider.connection, payer, mint, wallet, authority, amount);
  }

  public async depositLiquidity(
    input: Partial<DepositLiquidityArgs>
  ): Promise<void> {
    const user = input.user ?? Keypair.generate();
    const pool = input.pool ?? this.id.publicKey;
    const poolSignerPda = input.poolSignerPda ?? this.signerPda();
    const lpMint = input.lpMint ?? (await this.fetch()).mint;
    const lpTokenWallet =
      input.lpTokenWallet ??
      (await createAccount(provider.connection, payer, lpMint, user.publicKey));

    const defineMaxAmountTokens = async () => {
      const fetchPool = await this.fetch();
      const mint1 = fetchPool.reserves[0].mint;
      const mint2 = fetchPool.reserves[1].mint;

      const amountTokens: { mint: PublicKey; tokens: { amount: BN } }[] = [];
      amountTokens.push({ mint: mint1, tokens: { amount: new BN(100) } });
      amountTokens.push({ mint: mint2, tokens: { amount: new BN(10) } });

      return maxAmountTokens;
    };

    const maxAmountTokens =
      input.maxAmountTokens ?? (await defineMaxAmountTokens());

    const getVaultsAndWallets = async () => {
      const fetchPool = await this.fetch();

      const firstVault = fetchPool.reserves[0].vault;
      const secondVault = fetchPool.reserves[1].vault;

      const firstMint = fetchPool.reserves[0].mint;
      const secondMint = fetchPool.reserves[1].mint;

      const firstVaultAccount = await getAccount(
        provider.connection,
        firstVault
      );
      const secondVaultAccount = await getAccount(
        provider.connection,
        secondVault
      );

      const firstWalletAccount = await createAccount(
        provider.connection,
        payer,
        firstMint,
        user.publicKey
      );
      const secondWalletAccount = await createAccount(
        provider.connection,
        payer,
        secondMint,
        user.publicKey
      );

      return [
        {
          isSigner: false,
          isWritable: true,
          pubkey: firstVaultAccount.address,
        },
        {
          isSigner: false,
          isWritable: true,
          pubkey: firstWalletAccount,
        },
        {
          isSigner: false,
          isWritable: true,
          pubkey: secondVaultAccount.address,
        },
        {
          isSigner: false,
          isWritable: true,
          pubkey: secondWalletAccount,
        },
      ];
    };

    const vaultsAndWallets =
      input.vaultsAndWallets ?? (await getVaultsAndWallets());

    await amm.methods
      .depositLiquidity(maxAmountTokens)
      .accounts({
        user: user.publicKey,
        pool,
        poolSignerPda,
        lpMint,
        lpTokenWallet,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts(vaultsAndWallets)
      .signers([user])
      .rpc();
  }

  public async redeemLiquidity(
    input: Partial<RedeemLiquidityArgs>
  ): Promise<void> {
    const user = input.user ?? Keypair.generate();
    const pool = input.pool ?? this.id.publicKey;
    const poolSigner = input.poolSigner ?? this.signerPda();
    const lpMint = input.lpMint ?? (await this.fetch()).mint;
    const lpTokenWallet =
      input.lpTokenWallet ??
      (await createAccount(provider.connection, payer, lpMint, user.publicKey));

    const defineMinAmountTokens = async () => {
      const fetchPool = await this.fetch();
      const mint1 = fetchPool.reserves[0].mint;
      const mint2 = fetchPool.reserves[1].mint;

      const t: [PublicKey, { amount: BN }][] = [];
      t.push([mint1, { amount: new BN(0) }]);
      t.push([mint2, { amount: new BN(0) }]);

      return t;
    };

    const minAmountTokens =
      input.minAmountTokens ?? (await defineMinAmountTokens());

    const lpTokensToBurn = input.lpTokensToBurn ?? 100;

    const getVaultsAndWallets = async () => {
      const fetchPool = await this.fetch();

      const firstVault = fetchPool.reserves[0].vault;
      const secondVault = fetchPool.reserves[1].vault;

      const firstMint = fetchPool.reserves[0].mint;
      const secondMint = fetchPool.reserves[1].mint;

      const firstVaultAccount = await getAccount(
        provider.connection,
        firstVault
      );
      const secondVaultAccount = await getAccount(
        provider.connection,
        secondVault
      );

      const firstWalletAccount = await createAccount(
        provider.connection,
        payer,
        firstMint,
        user.publicKey
      );
      const secondWalletAccount = await createAccount(
        provider.connection,
        payer,
        secondMint,
        user.publicKey
      );

      return [
        {
          isSigner: false,
          isWritable: true,
          pubkey: firstVaultAccount.address,
        },
        {
          isSigner: false,
          isWritable: true,
          pubkey: firstWalletAccount,
        },
        {
          isSigner: false,
          isWritable: true,
          pubkey: secondVaultAccount.address,
        },
        {
          isSigner: false,
          isWritable: true,
          pubkey: secondWalletAccount,
        },
      ];
    };

    const vaultsAndWallets =
      input.vaultsAndWallets ?? (await getVaultsAndWallets());

    await amm.methods
      .redeemLiquidity({ amount: new BN(lpTokensToBurn) }, minAmountTokens)
      .accounts({
        user: user.publicKey,
        pool,
        poolSigner,
        lpMint,
        lpTokenWallet,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .remainingAccounts(vaultsAndWallets)
      .signers([user])
      .rpc();
  }

  public async swap(
    user: Keypair,
    sellWallet: PublicKey,
    buyWallet: PublicKey,
    sell: number,
    minBuy: number
  ) {
    const pool = await this.fetch();
    const getVaultOfWallet = async (wallet: PublicKey) => {
      const { mint } = await getAccount(provider.connection, wallet);
      const reserves = pool.reserves as any[];
      return reserves.find((r) => r.mint.toBase58() === mint.toBase58()).vault;
    };

    await amm.methods
      .swap({ amount: new BN(sell) }, { amount: new BN(minBuy) })
      .accounts({
        user: user.publicKey,
        discount: discountAddress(user.publicKey),
        sellWallet,
        sellVault: await getVaultOfWallet(sellWallet),
        buyWallet,
        buyVault: await getVaultOfWallet(buyWallet),
        pool: this.id.publicKey,
        poolSigner: this.signerPda(),
        programTollWallet: pool.programTollWallet,
        lpMint: pool.mint,
      })
      .signers([user])
      .rpc();
  }

  public async setSwapFee(permillion: number) {
    await amm.methods
      .setPoolSwapFee({
        permillion: new BN(permillion),
      })
      .accounts({ admin: this.admin.publicKey, pool: this.id.publicKey })
      .signers([this.admin])
      .rpc();
  }
}
