import chaiAsPromised from "chai-as-promised";
import chai from "chai";
chai.use(chaiAsPromised);

import * as createProgramToll from "./endpoints/create-program-toll";
import * as createDiscountSettings from "./endpoints/create-discount-settings";
import * as createPool from "./endpoints/create-pool";
import * as putDiscount from "./endpoints/put-discount";
import * as setPoolSwapFee from "./endpoints/set-pool-swap-fee";
import * as depositLiquidity from "./endpoints/deposit-liquidity";
import * as redeemLiquidity from "./endpoints/redeem-liquidity";
import * as swap from "./endpoints/swap";

import { airdrop, provider } from "../helpers";

describe("amm", () => {
  createProgramToll.test();
  createPool.test();
  createDiscountSettings.test();
  putDiscount.test();
  setPoolSwapFee.test();
  depositLiquidity.test();
  redeemLiquidity.test();
  swap.test();

  before("airdrop SOL to provider wallet", async () => {
    await airdrop(provider.wallet.publicKey);
  });
});
