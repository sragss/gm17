use core::ops::Mul;

use ark_ec::{pairing::Pairing, CurveGroup, scalar_mul::fixed_base::FixedBase};
use ark_ff::{Field, One, UniformRand, Zero};
use ark_poly::{EvaluationDomain, GeneralEvaluationDomain};
use ark_relations::r1cs::{
    ConstraintSynthesizer, ConstraintSystem, OptimizationGoal, Result as R1CSResult,
    SynthesisError, SynthesisMode,
};
use ark_std::{cfg_into_iter, cfg_iter, vec::Vec};

use ark_std::rand::Rng;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::{r1cs_to_sap::R1CStoSAP, ProvingKey, VerifyingKey};

/// Generates a random common reference string for
/// a circuit.
#[inline]
pub fn generate_random_parameters<E, C, R>(circuit: C, rng: &mut R) -> R1CSResult<ProvingKey<E>>
where
    E: Pairing,
    C: ConstraintSynthesizer<E::ScalarField>,
    R: Rng,
{
    let alpha = E::ScalarField::rand(rng);
    let beta = E::ScalarField::rand(rng);
    let gamma = E::ScalarField::one();
    let g = E::G1::rand(rng);
    let h = E::G2::rand(rng);

    generate_parameters::<E, C, R>(circuit, alpha, beta, gamma, g, h, rng)
}

/// Create parameters for a circuit, given some toxic waste.
pub fn generate_parameters<E, C, R>(
    circuit: C,
    alpha: E::ScalarField,
    beta: E::ScalarField,
    gamma: E::ScalarField,
    g: E::G1,
    h: E::G2,
    rng: &mut R,
) -> R1CSResult<ProvingKey<E>>
where
    E: Pairing,
    C: ConstraintSynthesizer<E::ScalarField>,
    R: Rng,
{
    type D<F> = GeneralEvaluationDomain<F>;

    let setup_time = start_timer!(|| "GrothMaller17::Generator");
    let cs = ConstraintSystem::new_ref();
    cs.set_optimization_goal(OptimizationGoal::Constraints);
    cs.set_mode(SynthesisMode::Setup);

    // Synthesize the circuit.
    let synthesis_time = start_timer!(|| "Constraint synthesis");
    circuit.generate_constraints(cs.clone())?;
    end_timer!(synthesis_time);

    let lc_time = start_timer!(|| "Inlining LCs");
    cs.finalize();
    end_timer!(lc_time);

    let num_inputs = cs.num_instance_variables();
    let num_constraints = cs.num_constraints();

    ///////////////////////////////////////////////////////////////////////////
    let domain_time = start_timer!(|| "Constructing evaluation domain");

    let domain_size = 2 * num_constraints + 2 * num_inputs - 1;
    let domain = D::new(domain_size).ok_or(SynthesisError::PolynomialDegreeTooLarge)?;
    let t = domain.sample_element_outside_domain(rng);

    end_timer!(domain_time);
    ///////////////////////////////////////////////////////////////////////////

    let reduction_time = start_timer!(|| "R1CS to SAP Instance Map with Evaluation");
    let (a, c, zt, sap_num_variables, m_raw) =
        R1CStoSAP::instance_map_with_evaluation::<E::ScalarField, D<E::ScalarField>>(cs.clone(), &t)?;
    end_timer!(reduction_time);
    drop(cs);

    // Compute query densities
    let non_zero_a = cfg_into_iter!(0..sap_num_variables)
        .map(|i| (!a[i].is_zero()) as usize)
        .sum();
    // let scalar_bits = E::ScalarField::size_in_bits();
    let scalar_bits = 12;

    // Compute G window table
    let g_window_time = start_timer!(|| "Compute G window table");
    let g_window = FixedBase::get_mul_window_size(
        // Verifier query
        num_inputs
        // A query
        + non_zero_a
        // C query 1
        + (sap_num_variables - (num_inputs - 1))
        // C query 2
        + sap_num_variables + 1
        // G gamma2 Z t
        + m_raw + 1,
    );
    let g_table = FixedBase::get_window_table::<E::G1>(scalar_bits, g_window, g);
    end_timer!(g_window_time);

    // Generate the R1CS proving key
    let proving_key_time = start_timer!(|| "Generate the R1CS proving key");

    // Compute the A-query
    let a_time = start_timer!(|| "Calculate A");
    let a_query = FixedBase::msm::<E::G1>(
        scalar_bits,
        g_window,
        &g_table,
        &cfg_iter!(a).map(|a| *a * &gamma).collect::<Vec<_>>(),
    );
    end_timer!(a_time);

    // Compute the G_gamma-query
    let g_gamma_time = start_timer!(|| "Calculate G gamma");
    let gamma_z = zt * &gamma;
    let alpha_beta = alpha + &beta;
    let ab_gamma_z = alpha_beta * &gamma * &zt;
    let g_gamma = g.mul(&gamma);
    let g_gamma_z = g.mul(&gamma_z);
    let h_gamma = h.mul(&gamma);
    let h_gamma_z = h_gamma.mul(&zt);
    let g_ab_gamma_z = g.mul(&ab_gamma_z);
    let g_gamma2_z2 = g.mul(&gamma_z.square());

    // Compute the vector G_gamma2_z_t := Z(t) * t^i * gamma^2 * G
    let gamma2_z_t = gamma_z * &gamma;
    let g_gamma2_z_t = FixedBase::msm::<E::G1>(
        scalar_bits,
        g_window,
        &g_table,
        &cfg_into_iter!(0..m_raw + 1)
            .map(|i| gamma2_z_t * &(t.pow([i as u64])))
            .collect::<Vec<_>>(),
    );
    end_timer!(g_gamma_time);

    // Compute the C_1-query
    let c1_time = start_timer!(|| "Calculate C1");
    let result = FixedBase::msm::<E::G1>(
        scalar_bits,
        g_window,
        &g_table,
        &cfg_into_iter!(0..sap_num_variables + 1)
            .map(|i| c[i] * &gamma + &(a[i] * &alpha_beta))
            .collect::<Vec<_>>(),
    );
    let (verifier_query, c_query_1) = result.split_at(num_inputs);
    end_timer!(c1_time);

    // Compute the C_2-query
    let c2_time = start_timer!(|| "Calculate C2");
    let double_gamma2_z = (zt * &gamma.square()).double();
    let c_query_2 = FixedBase::msm::<E::G1>(
        scalar_bits,
        g_window,
        &g_table,
        &cfg_into_iter!(0..sap_num_variables + 1)
            .map(|i| a[i] * &double_gamma2_z)
            .collect::<Vec<_>>(),
    );
    drop(g_table);
    end_timer!(c2_time);

    // Compute H_gamma window table
    let h_gamma_time = start_timer!(|| "Compute H table");
    let h_gamma_window = FixedBase::get_mul_window_size(non_zero_a);
    let h_gamma_table =
        FixedBase::get_window_table::<E::G2>(scalar_bits, h_gamma_window, h_gamma);
    end_timer!(h_gamma_time);

    // Compute the B-query
    let b_time = start_timer!(|| "Calculate B");
    let b_query = FixedBase::msm::<E::G2>(
        scalar_bits,
        h_gamma_window,
        &h_gamma_table,
        &a,
    );
    drop(h_gamma_table);
    end_timer!(b_time);

    end_timer!(proving_key_time);

    // Generate R1CS verification key
    let verifying_key_time = start_timer!(|| "Generate the R1CS verification key");
    let g_alpha = g.mul(&alpha);
    let h_beta = h.mul(&beta);
    end_timer!(verifying_key_time);

    let vk = VerifyingKey::<E> {
        h_g2: h.into_affine(),
        g_alpha_g1: g_alpha.into_affine(),
        h_beta_g2: h_beta.into_affine(),
        g_gamma_g1: g_gamma.into_affine(),
        h_gamma_g2: h_gamma.into_affine(),
        query: E::G1::normalize_batch(&verifier_query),
    };

    let batch_normalization_time = start_timer!(|| "Convert proving key elements to affine");
    let a_query = E::G1::normalize_batch(&a_query);
    let b_query = E::G2::normalize_batch(&b_query);
    let c_query_1 = E::G1::normalize_batch(&c_query_1);
    let c_query_2 = E::G1::normalize_batch(&c_query_2);
    let g_gamma2_z_t = E::G1::normalize_batch(&g_gamma2_z_t);
    end_timer!(batch_normalization_time);

    end_timer!(setup_time);

    Ok(ProvingKey {
        vk,
        a_query,
        b_query,
        c_query_1,
        c_query_2,
        g_gamma_z: g_gamma_z.into_affine(),
        h_gamma_z: h_gamma_z.into_affine(),
        g_ab_gamma_z: g_ab_gamma_z.into_affine(),
        g_gamma2_z2: g_gamma2_z2.into_affine(),
        g_gamma2_z_t,
    })
}
