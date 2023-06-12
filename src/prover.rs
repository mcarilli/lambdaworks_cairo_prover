use super::{
    air::{constraints::evaluator::ConstraintEvaluator, frame::Frame, trace::TraceTable},
    fri::fri_commit_phase,
    sample_z_ood,
};
use crate::{
    air::traits::AIR,
    batch_sample_challenges,
    fri::{fri_decommit::FriDecommitment, fri_query_phase, HASHER},
    proof::{DeepPolynomialOpenings, StarkProof},
    transcript_to_field, Domain,
    verifier::composition_poly_ood_evaluation_exact_from_trace,
};
#[cfg(not(feature = "test_fiat_shamir"))]
use lambdaworks_crypto::fiat_shamir::default_transcript::DefaultTranscript;
use lambdaworks_crypto::{fiat_shamir::transcript::Transcript, merkle_tree::merkle::MerkleTree};

#[cfg(feature = "test_fiat_shamir")]
use lambdaworks_crypto::fiat_shamir::test_transcript::TestTranscript;

use lambdaworks_fft::{errors::FFTError, polynomial::FFTPoly};
use lambdaworks_math::{
    field::{element::FieldElement, traits::IsFFTField},
    polynomial::Polynomial,
    traits::ByteConversion,
};
use log::info;

#[cfg(debug_assertions)]
use crate::air::debug::validate_trace;

#[derive(Debug)]
pub enum ProvingError {
    WrongParameter(String),
}

struct Round1<F: IsFFTField, A: AIR<Field = F>> {
    trace_polys: Vec<Polynomial<FieldElement<F>>>,
    lde_trace: TraceTable<F>,
    lde_trace_merkle_trees: Vec<MerkleTree<F>>,
    lde_trace_merkle_roots: Vec<FieldElement<F>>,
    rap_challenges: A::RAPChallenges,
}

struct Round2<F: IsFFTField> {
    composition_poly_even: Polynomial<FieldElement<F>>,
    lde_composition_poly_even_evaluations: Vec<FieldElement<F>>,
    composition_poly_even_merkle_tree: MerkleTree<F>,
    composition_poly_even_root: FieldElement<F>,
    composition_poly_odd: Polynomial<FieldElement<F>>,
    lde_composition_poly_odd_evaluations: Vec<FieldElement<F>>,
    composition_poly_odd_merkle_tree: MerkleTree<F>,
    composition_poly_odd_root: FieldElement<F>,
}

struct Round3<F: IsFFTField> {
    trace_ood_frame_evaluations: Frame<F>,
    composition_poly_even_ood_evaluation: FieldElement<F>,
    composition_poly_odd_ood_evaluation: FieldElement<F>,
}

struct Round4<F: IsFFTField> {
    fri_last_value: FieldElement<F>,
    fri_layers_merkle_roots: Vec<FieldElement<F>>,
    deep_poly_openings: DeepPolynomialOpenings<F>,
    query_list: Vec<FriDecommitment<F>>,
}

#[cfg(feature = "test_fiat_shamir")]
fn round_0_transcript_initialization() -> TestTranscript {
    TestTranscript::new()
}

#[cfg(not(feature = "test_fiat_shamir"))]
fn round_0_transcript_initialization() -> DefaultTranscript {
    // TODO: add strong fiat shamir
    DefaultTranscript::new()
}

fn batch_commit<F>(
    vectors: Vec<&Vec<FieldElement<F>>>,
) -> (Vec<MerkleTree<F>>, Vec<FieldElement<F>>)
where
    F: IsFFTField,
    FieldElement<F>: ByteConversion,
{
    let trees: Vec<_> = vectors
        .iter()
        .map(|col| MerkleTree::build(col, Box::new(HASHER)))
        .collect();

    let roots = trees.iter().map(|tree| tree.root.clone()).collect();
    (trees, roots)
}

fn evaluate_polynomial_on_lde_domain<F>(
    p: &Polynomial<FieldElement<F>>,
    blowup_factor: usize,
    domain_size: usize,
    offset: &FieldElement<F>,
) -> Result<Vec<FieldElement<F>>, FFTError>
where
    F: IsFFTField,
    Polynomial<FieldElement<F>>: FFTPoly<F>,
{
    // Evaluate those polynomials t_j on the large domain D_LDE.
    let evaluations = p.evaluate_offset_fft(blowup_factor, Some(domain_size), offset)?;
    let step = evaluations.len() / (domain_size * blowup_factor);
    match step {
        1 => Ok(evaluations),
        _ => Ok(evaluations.into_iter().step_by(step).collect()),
    }
}

#[allow(clippy::type_complexity)]
fn interpolate_and_commit<T, F>(
    trace: &TraceTable<F>,
    domain: &Domain<F>,
    transcript: &mut T,
) -> (
    Vec<Polynomial<FieldElement<F>>>,
    Vec<Vec<FieldElement<F>>>,
    Vec<MerkleTree<F>>,
    Vec<FieldElement<F>>,
)
where
    T: Transcript,
    F: IsFFTField,
    FieldElement<F>: ByteConversion,
{
    let trace_polys = trace.compute_trace_polys();

    // Evaluate those polynomials t_j on the large domain D_LDE.
    let lde_trace_evaluations = trace_polys
        .iter()
        .map(|poly| {
            evaluate_polynomial_on_lde_domain(
                poly,
                domain.blowup_factor,
                domain.interpolation_domain_size,
                &domain.coset_offset,
            )
        })
        .collect::<Result<Vec<Vec<FieldElement<F>>>, FFTError>>()
        .unwrap();

    // Compute commitments [t_j].
    let lde_trace = TraceTable::new_from_cols(&lde_trace_evaluations);
    let (lde_trace_merkle_trees, lde_trace_merkle_roots) =
        batch_commit(lde_trace.cols().iter().collect());

    // >>>> Send commitments: [tⱼ]
    for root in lde_trace_merkle_roots.iter() {
        transcript.append(&root.to_bytes_be());
    }

    (
        trace_polys,
        lde_trace_evaluations,
        lde_trace_merkle_trees,
        lde_trace_merkle_roots,
    )
}

fn round_1_randomized_air_with_preprocessing<F: IsFFTField, A: AIR<Field = F>, T: Transcript>(
    air: &A,
    raw_trace: &A::RawTrace,
    domain: &Domain<F>,
    public_input: &mut A::PublicInput,
    transcript: &mut T,
) -> Result<Round1<F, A>, ProvingError>
where
    FieldElement<F>: ByteConversion,
{
    let main_trace = air.build_main_trace(raw_trace, public_input)?;

    let (mut trace_polys, mut evaluations, mut lde_trace_merkle_trees, mut lde_trace_merkle_roots) =
        interpolate_and_commit(&main_trace, domain, transcript);

    println!("trace_polys[0].coefficients.len() {}", trace_polys[0].coefficients.len());

    let rap_challenges = air.build_rap_challenges(transcript);

    let aux_trace = air.build_auxiliary_trace(&main_trace, &rap_challenges, public_input);

    println!("aux_trace.is_empty() {}", aux_trace.is_empty());

    if !aux_trace.is_empty() {
        // Check that this is valid for interpolation
        let (aux_trace_polys, aux_trace_polys_evaluations, aux_merkle_trees, aux_merkle_roots) =
            interpolate_and_commit(&aux_trace, domain, transcript);
        trace_polys.extend_from_slice(&aux_trace_polys);
        evaluations.extend_from_slice(&aux_trace_polys_evaluations);
        lde_trace_merkle_trees.extend_from_slice(&aux_merkle_trees);
        lde_trace_merkle_roots.extend_from_slice(&aux_merkle_roots);
    }

    let lde_trace = TraceTable::new_from_cols(&evaluations);

    Ok(Round1 {
        trace_polys,
        lde_trace,
        lde_trace_merkle_roots,
        lde_trace_merkle_trees,
        rap_challenges,
    })
}

fn round_2_compute_composition_polynomial<F, A>(
    air: &A,
    domain: &Domain<F>,
    round_1_result: &Round1<F, A>,
    public_input: &A::PublicInput,
    transition_coeffs: &[(FieldElement<F>, FieldElement<F>)],
    boundary_coeffs: &[(FieldElement<F>, FieldElement<F>)],
) -> Round2<F>
where
    F: IsFFTField,
    A: AIR<Field = F>,
    FieldElement<F>: ByteConversion,
{
    // Create evaluation table
    let evaluator = ConstraintEvaluator::new(
        air,
        &round_1_result.trace_polys,
        &domain.trace_primitive_root,
        public_input,
        &round_1_result.rap_challenges,
    );

    let constraint_evaluations = evaluator.evaluate(
        &round_1_result.lde_trace,
        &domain.lde_roots_of_unity_coset,
        transition_coeffs,
        boundary_coeffs,
        &round_1_result.rap_challenges,
    );

    // Get the composition poly H
    // https://lambdaclass.github.io/lambdaworks/proving_systems/starks/recap.html#consistency-check
    // H Consistency Note 1:
    // H is computed by FFT-interpolating evals of RHS(B(t(x)), C(t(x)), x) on the lde domain*.
    // So the computed H is unavoidably a polynomial of bounded degree.
    // But if the constraints are not satisfied, RHS(x) is not a polynomial of bounded degree.
    // Therefore, when we later sample a random point z and compare the (interpolation-derived) H(z)
    // with the (exact) RHS(B(t(z), C(t(z)), z), they will not match.
    //
    // *In our case for an honest proof i think the full lde domain is more points than needed.
    // In our case deg(H) <= trace len so a domain the size of the trace domain would suffice.
    // It would still need to be a coset domain, to avoid zeros in denoms
    // (e.g. a coarser LDE domain would work).
    let composition_poly = constraint_evaluations.compute_composition_poly(&domain.coset_offset);
    println!("composition_poly.coefficients.len() {}", composition_poly.coefficients.len());
    let (composition_poly_even, composition_poly_odd) = composition_poly.even_odd_decomposition();

    let lde_composition_poly_even_evaluations = evaluate_polynomial_on_lde_domain(
        &composition_poly_even,
        domain.blowup_factor,
        domain.interpolation_domain_size,
        &domain.coset_offset,
    )
    .unwrap();
    let lde_composition_poly_odd_evaluations = evaluate_polynomial_on_lde_domain(
        &composition_poly_odd,
        domain.blowup_factor,
        domain.interpolation_domain_size,
        &domain.coset_offset,
    )
    .unwrap();

    let (composition_poly_merkle_trees, composition_poly_roots) = batch_commit(vec![
        &lde_composition_poly_even_evaluations,
        &lde_composition_poly_odd_evaluations,
    ]);

    Round2 {
        composition_poly_even,
        lde_composition_poly_even_evaluations,
        composition_poly_even_merkle_tree: composition_poly_merkle_trees[0].clone(),
        composition_poly_even_root: composition_poly_roots[0].clone(),
        composition_poly_odd,
        lde_composition_poly_odd_evaluations,
        composition_poly_odd_merkle_tree: composition_poly_merkle_trees[1].clone(),
        composition_poly_odd_root: composition_poly_roots[1].clone(),
    }
}

fn round_3_evaluate_polynomials_in_out_of_domain_element<F: IsFFTField, A: AIR<Field = F>>(
    air: &A,
    domain: &Domain<F>,
    public_input: &A::PublicInput,
    round_1_result: &Round1<F, A>,
    round_2_result: &Round2<F>,
    z: &FieldElement<F>,
    rap_challenges: &A::RAPChallenges,
    boundary_coeffs: &[(FieldElement<F>, FieldElement<F>)],
    transition_coeffs: &[(FieldElement<F>, FieldElement<F>)],
    evil: bool,
) -> Round3<F>
where
    FieldElement<F>: ByteConversion,
{
    // Returns the Out of Domain Frame for the given trace polynomials, out of domain evaluation point (called `z` in the literature),
    // frame offsets given by the AIR and primitive root used for interpolating the trace polynomials.
    // An out of domain frame is nothing more than the evaluation of the trace polynomials in the points required by the
    // verifier to check the consistency between the trace and the composition polynomial.
    //
    // In the fibonacci example, the ood frame is simply the evaluations `[t(z), t(z * g), t(z * g^2)]`, where `t` is the trace
    // polynomial and `g` is the primitive root of unity used when interpolating `t`.
    //
    // H Consistency Note 2:
    // ...but if we're faking a proof, what if we just ignore our interpolation-derived H(z)
    // and submit a H_claimed(z) == (exact) RHS(z)?
    // Then in theory Deep(x) = gamma_1 * (H(x) - H_claimed(z) / (x - z)) + ...
    // is not a low-degree polynomial, so the FRI check on Deep(x) should fail.
    let ood_trace_evaluations = Frame::get_trace_evaluations(
        &round_1_result.trace_polys,
        z,
        &air.context().transition_offsets,
        &domain.trace_primitive_root,
    );
    let trace_ood_frame_evaluations = Frame::new(
        ood_trace_evaluations.into_iter().flatten().collect(),
        round_1_result.trace_polys.len(),
    );

    let z_squared = z * z;

    // Evaluate H_1 and H_2 in z^2.
    let (composition_poly_even_ood_evaluation, composition_poly_odd_ood_evaluation) = if evil {
        let H_z_exact_from_trace = composition_poly_ood_evaluation_exact_from_trace(
            air,
            &trace_ood_frame_evaluations,
            domain,
            public_input,
            &z,
            &rap_challenges,
            boundary_coeffs,
            transition_coeffs,
        );
        (H_z_exact_from_trace, FieldElement::<F>::from(0))
    } else {
        (round_2_result.composition_poly_even.evaluate(&z_squared),
            round_2_result.composition_poly_odd.evaluate(&z_squared))
    };

    Round3 {
        trace_ood_frame_evaluations,
        composition_poly_even_ood_evaluation,
        composition_poly_odd_ood_evaluation,
    }
}

fn round_4_compute_and_run_fri_on_the_deep_composition_polynomial<
    F: IsFFTField,
    A: AIR<Field = F>,
    T: Transcript,
>(
    air: &A,
    domain: &Domain<F>,
    round_1_result: &Round1<F, A>,
    round_2_result: &Round2<F>,
    round_3_result: &Round3<F>,
    z: &FieldElement<F>,
    transcript: &mut T,
    evil: bool,
    bad_trace: bool,
) -> Round4<F>
where
    FieldElement<F>: ByteConversion,
{
    // <<<< Receive challenges: 𝛾, 𝛾'
    let composition_poly_coeffients = [
        transcript_to_field(transcript),
        transcript_to_field(transcript),
    ];
    // <<<< Receive challenges: 𝛾ⱼ, 𝛾ⱼ'
    let trace_poly_coeffients = batch_sample_challenges::<F, T>(
        air.context().transition_offsets.len() * air.context().trace_columns,
        transcript,
    );

    // Compute p₀ (deep composition polynomial)
    let deep_composition_poly = compute_deep_composition_poly(
        air,
        domain,
        &round_1_result.trace_polys,
        round_2_result,
        round_3_result,
        z,
        &domain.trace_primitive_root,
        &composition_poly_coeffients,
        &trace_poly_coeffients,
        evil,
        bad_trace,
    );

    // FRI commit and query phases
    let (fri_last_value, fri_layers) = fri_commit_phase(
        domain.root_order as usize,
        deep_composition_poly,
        &domain.lde_roots_of_unity_coset,
        transcript,
    );
    let (query_list, iota_0) = fri_query_phase(air, domain, &fri_layers, transcript);
    println!("iota_0 {}", iota_0);

    let fri_layers_merkle_roots: Vec<_> = fri_layers
        .iter()
        .map(|layer| layer.merkle_tree.root.clone())
        .collect();

    let deep_poly_openings =
        open_deep_composition_poly(domain, round_1_result, round_2_result, iota_0);

    Round4 {
        fri_last_value,
        fri_layers_merkle_roots,
        deep_poly_openings,
        query_list,
    }
}

fn interp_from_num_denom<F: IsFFTField>(
    num: &Polynomial<FieldElement<F>>,
    denom: &Polynomial<FieldElement<F>>,
    domain: &Domain<F>,
    poly_sanity_check: &Polynomial<FieldElement<F>>,
    evil: bool,
    bad_trace: bool,
) -> Polynomial<FieldElement<F>> {
    let target_deg = if evil || !bad_trace {
        domain.lde_roots_of_unity_coset.len() / domain.blowup_factor as usize
    } else {
        domain.lde_roots_of_unity_coset.len() / 2 as usize
    };
    let num_evals = evaluate_polynomial_on_lde_domain(
        &num, domain.blowup_factor, domain.interpolation_domain_size, &domain.coset_offset).unwrap();
    let denom_evals = evaluate_polynomial_on_lde_domain(
        &denom, domain.blowup_factor, domain.interpolation_domain_size, &domain.coset_offset).unwrap();
    let evals: Vec<_> = num_evals.iter().zip(denom_evals).map(|(num, denom)| num / denom).collect();
    // [..target_deg + 1] yields num_pwns=0 and "step 3 failed" in each fuzzing attempt
    // so FRI appears strong enough to reject polys whose degree is even slightly too high
    let result = Polynomial::interpolate(
        &domain.lde_roots_of_unity_coset[..target_deg], &evals[..target_deg]).unwrap();
    println!("num.coefficients.len(), denom.coefficients.len(), result.coefficients.len() = {}, {}, {}",
        num.coefficients.len(), denom.coefficients.len(), result.coefficients.len());
    // sanity checks that interpolated poly has the expected relationship to non-interpreted poly
    if !evil {
        for (coeff_interp, coeff) in result.coefficients.iter().zip(&poly_sanity_check.coefficients) {
            assert_eq!(coeff_interp, coeff);
        }
    }
    result
}

/// Returns the DEEP composition polynomial that the prover then commits to using
/// FRI. This polynomial is a linear combination of the trace polynomial and the
/// composition polynomial, with coefficients sampled by the verifier (i.e. using Fiat-Shamir).
#[allow(clippy::too_many_arguments)]
fn compute_deep_composition_poly<A: AIR, F: IsFFTField>(
    air: &A,
    domain: &Domain<F>,
    trace_polys: &[Polynomial<FieldElement<F>>],
    round_2_result: &Round2<F>,
    round_3_result: &Round3<F>,
    z: &FieldElement<F>,
    primitive_root: &FieldElement<F>,
    composition_poly_gammas: &[FieldElement<F>; 2],
    trace_terms_gammas: &[FieldElement<F>],
    evil: bool,
    bad_trace: bool,
) -> Polynomial<FieldElement<F>> {
    // Compute composition polynomial terms of the deep composition polynomial.
    let x = Polynomial::new_monomial(FieldElement::one(), 1);
    let h_1 = &round_2_result.composition_poly_even;
    let h_1_z2 = &round_3_result.composition_poly_even_ood_evaluation;
    let h_2 = &round_2_result.composition_poly_odd;
    let h_2_z2 = &round_3_result.composition_poly_odd_ood_evaluation;
    let gamma = &composition_poly_gammas[0];
    let gamma_p = &composition_poly_gammas[1];
    let z_squared = z * z;

    // 𝛾 ( H₁ − H₁(z²) ) / ( X − z² )
    let h_1_term = gamma * (h_1 - h_1_z2) / (&x - &z_squared);
    let h_1_num = gamma * (h_1 - h_1_z2);
    let h_1_denom = &x - &z_squared;
    let h_1_from_interp = interp_from_num_denom(
        &h_1_num,
        &h_1_denom,
        domain,
        &h_1_term,
        evil,
        bad_trace);
    println!("evil {} bad_trace {}", evil, bad_trace);
    println!("h_1.coefficients.len() {}", h_1.coefficients.len());
    println!("h_1_term.coefficients.len() {}", h_1_term.coefficients.len());
    println!("h_1_from_interp.coefficientsl.len() {}", h_1_from_interp.coefficients.len());

    // 𝛾' ( H₂ − H₂(z²) ) / ( X − z² )
    let h_2_term = gamma_p * (h_2 - h_2_z2) / (&x - &z_squared);

    let h_2_num = gamma_p * (h_2 - h_2_z2);
    let h_2_denom = &x - &z_squared;
    let h_2_from_interp = interp_from_num_denom(
        &h_2_num,
        &h_2_denom,
        domain,
        &h_2_term,
        evil,
        bad_trace,
    );

    // Get trace evaluations needed for the trace terms of the deep composition polynomial
    let transition_offsets = air.context().transition_offsets;
    let trace_frame_evaluations =
        Frame::get_trace_evaluations(trace_polys, z, &transition_offsets, primitive_root);

    // Compute the sum of all the trace terms of the deep composition polynomial.
    // There is one term for every trace polynomial and for every row in the frame.
    // ∑ ⱼₖ [ 𝛾ₖ ( tⱼ − tⱼ(z) ) / ( X − zgᵏ )]
    let mut trace_terms = Polynomial::zero();
    let mut trace_terms_from_interp = Polynomial::<FieldElement<F>>::zero();
    for (i, t_j) in trace_polys.iter().enumerate() {
        for (j, (evaluations, offset)) in trace_frame_evaluations
            .iter()
            .zip(&transition_offsets)
            .enumerate()
        {
            let t_j_z = evaluations[i].clone();
            let z_shifted = z * primitive_root.pow(*offset);
            let poly = (t_j - &t_j_z) / (&x - &z_shifted);

            let poly_num = t_j - t_j_z;
            let poly_denom = &x - &z_shifted;
            let poly_from_interp = interp_from_num_denom(
                &poly_num,
                &poly_denom,
                domain,
                &poly,
                evil,
                bad_trace
            );
 
            trace_terms =
                trace_terms + poly * &trace_terms_gammas[i * trace_frame_evaluations.len() + j];

            trace_terms_from_interp =
                trace_terms_from_interp + poly_from_interp * &trace_terms_gammas[i * trace_frame_evaluations.len() + j];
        }
    }

    let deep = h_1_term + h_2_term + &trace_terms;
    // I don't think trace terms need the evil interpolation, they should be low degree even for a malicious trace
    let deep_from_interp = h_1_from_interp + h_2_from_interp + trace_terms;
    // let deep_from_interp = h_1_from_interp + h_2_from_interp + trace_terms_from_interp;
    if evil {
        println!("deep_from_interp.coefficients.len() {}", deep_from_interp.coefficients.len());
        println!("deep.coefficients.len() {}", deep.coefficients.len());
        deep_from_interp
    } else {
        deep
    }
}

fn open_deep_composition_poly<F: IsFFTField, A: AIR<Field = F>>(
    domain: &Domain<F>,
    round_1_result: &Round1<F, A>,
    round_2_result: &Round2<F>,
    index_to_open: usize,
) -> DeepPolynomialOpenings<F>
where
    FieldElement<F>: ByteConversion,
{
    let index = index_to_open % domain.lde_roots_of_unity_coset.len();

    // H₁ openings
    let lde_composition_poly_even_proof = round_2_result
        .composition_poly_even_merkle_tree
        .get_proof_by_pos(index)
        .unwrap();
    let lde_composition_poly_even_evaluation =
        round_2_result.lde_composition_poly_even_evaluations[index].clone();

    // H₂ openings
    let lde_composition_poly_odd_proof = round_2_result
        .composition_poly_odd_merkle_tree
        .get_proof_by_pos(index)
        .unwrap();
    let lde_composition_poly_odd_evaluation =
        round_2_result.lde_composition_poly_odd_evaluations[index].clone();

    // Trace polynomials openings
    let lde_trace_merkle_proofs = round_1_result
        .lde_trace_merkle_trees
        .iter()
        .map(|tree| tree.get_proof_by_pos(index).unwrap())
        .collect();
    let lde_trace_evaluations = round_1_result.lde_trace.get_row(index).to_vec();

    DeepPolynomialOpenings {
        lde_composition_poly_even_proof,
        lde_composition_poly_even_evaluation,
        lde_composition_poly_odd_proof,
        lde_composition_poly_odd_evaluation,
        lde_trace_merkle_proofs,
        lde_trace_evaluations,
    }
}

// https://doc.rust-lang.org/reference/items/associated-items.html#associated-constants-examples
struct NotEvil;
struct Evil;
trait EvilOrNot {
    const IS_EVIL: bool;
}
impl EvilOrNot for NotEvil {
    const IS_EVIL: bool = false;
}
impl EvilOrNot for Evil {
    const IS_EVIL: bool = true;
}
// ^ this doesn't help, compiler doesn't let me specify default for a function's (prove's) generic type parameter
// pub fn prove<F: IsFFTField, A: AIR<Field = F>, E = NotEvil>(

// FIXME remove unwrap() calls and return errors
pub fn prove<F: IsFFTField, A: AIR<Field = F>>(
    trace: &A::RawTrace,
    air: &A,
    public_input: &mut A::PublicInput,
    evil: bool,
    bad_trace: bool,
) -> Result<StarkProof<F>, ProvingError>
where
    FieldElement<F>: ByteConversion,
{
    info!("Starting proof generation...");

    let domain = Domain::new(air);

    println!("domain.root_order {}", domain.root_order);
    println!("domain.lde_roots_of_unity_coset.len() {}", domain.lde_roots_of_unity_coset.len());
    println!("domain.interpolation_domain_size {}", domain.interpolation_domain_size);

    let mut transcript = round_0_transcript_initialization();

    // ===================================
    // ==========|   Round 1   |==========
    // ===================================

    let round_1_result = round_1_randomized_air_with_preprocessing::<F, A, _>(
        air,
        trace,
        &domain,
        public_input,
        &mut transcript,
    )?;

    #[cfg(debug_assertions)]
    validate_trace(
        air,
        &round_1_result.trace_polys,
        &domain,
        public_input,
        &round_1_result.rap_challenges,
    );

    // ===================================
    // ==========|   Round 2   |==========
    // ===================================

    // <<<< Receive challenges: 𝛼_j^B
    let boundary_coeffs_alphas =
        batch_sample_challenges(round_1_result.trace_polys.len(), &mut transcript);
    // <<<< Receive challenges: 𝛽_j^B
    let boundary_coeffs_betas =
        batch_sample_challenges(round_1_result.trace_polys.len(), &mut transcript);
    // <<<< Receive challenges: 𝛼_j^T
    let transition_coeffs_alphas =
        batch_sample_challenges(air.context().num_transition_constraints, &mut transcript);
    // <<<< Receive challenges: 𝛽_j^T
    let transition_coeffs_betas =
        batch_sample_challenges(air.context().num_transition_constraints, &mut transcript);

    let boundary_coeffs: Vec<_> = boundary_coeffs_alphas
        .into_iter()
        .zip(boundary_coeffs_betas)
        .collect();
    let transition_coeffs: Vec<_> = transition_coeffs_alphas
        .into_iter()
        .zip(transition_coeffs_betas)
        .collect();

    // boundary_coeffs[0] is (FieldElement<_>, FieldElement<_>)
    // println!("{}", boundary_coeffs[0].0);

    let round_2_result = round_2_compute_composition_polynomial(
        air,
        &domain,
        &round_1_result,
        public_input,
        &transition_coeffs,
        &boundary_coeffs,
    );

    // >>>> Send commitments: [H₁], [H₂]
    transcript.append(&round_2_result.composition_poly_even_root.to_bytes_be());
    transcript.append(&round_2_result.composition_poly_odd_root.to_bytes_be());

    // ===================================
    // ==========|   Round 3   |==========
    // ===================================

    // <<<< Receive challenge: z
    let z = sample_z_ood(
        &domain.lde_roots_of_unity_coset,
        &domain.trace_roots_of_unity,
        &mut transcript,
    );

    let round_3_result = round_3_evaluate_polynomials_in_out_of_domain_element(
        air,
        &domain,
        public_input,
        &round_1_result,
        &round_2_result,
        &z,
        &round_1_result.rap_challenges,
        &boundary_coeffs,
        &transition_coeffs,
        evil,
    );

    // >>>> Send value: H₁(z²)
    transcript.append(
        &round_3_result
            .composition_poly_even_ood_evaluation
            .to_bytes_be(),
    );

    // >>>> Send value: H₂(z²)
    transcript.append(
        &round_3_result
            .composition_poly_odd_ood_evaluation
            .to_bytes_be(),
    );
    // >>>> Send values: tⱼ(zgᵏ)
    for i in 0..round_3_result.trace_ood_frame_evaluations.num_rows() {
        for element in round_3_result.trace_ood_frame_evaluations.get_row(i).iter() {
            transcript.append(&element.to_bytes_be());
        }
    }

    // ===================================
    // ==========|   Round 4   |==========
    // ===================================

    // Part of this round is running FRI, which is an interactive
    // protocol on its own. Therefore we pass it the transcript
    // to simulate the interactions with the verifier.
    let round_4_result = round_4_compute_and_run_fri_on_the_deep_composition_polynomial(
        air,
        &domain,
        &round_1_result,
        &round_2_result,
        &round_3_result,
        &z,
        &mut transcript,
        evil,
        bad_trace,
    );

    info!("End proof generation");

    Ok(StarkProof {
        // [tⱼ]
        lde_trace_merkle_roots: round_1_result.lde_trace_merkle_roots,
        // tⱼ(zgᵏ)
        trace_ood_frame_evaluations: round_3_result.trace_ood_frame_evaluations,
        // [H₁]
        composition_poly_even_root: round_2_result.composition_poly_even_root,
        // H₁(z²)
        composition_poly_even_ood_evaluation: round_3_result.composition_poly_even_ood_evaluation,
        // [H₂]
        composition_poly_odd_root: round_2_result.composition_poly_odd_root,
        // H₂(z²)
        composition_poly_odd_ood_evaluation: round_3_result.composition_poly_odd_ood_evaluation,
        // [pₖ]
        fri_layers_merkle_roots: round_4_result.fri_layers_merkle_roots,
        // pₙ
        fri_last_value: round_4_result.fri_last_value,
        // Open(p₀(D₀), 𝜐ₛ), Open(pₖ(Dₖ), −𝜐ₛ^(2ᵏ))
        query_list: round_4_result.query_list,
        // Open(H₁(D_LDE, 𝜐₀), Open(H₂(D_LDE, 𝜐₀), Open(tⱼ(D_LDE), 𝜐₀)
        deep_poly_openings: round_4_result.deep_poly_openings,
    })
}

#[cfg(test)]
mod tests {
    use lambdaworks_math::{
        field::{
            element::FieldElement, fields::fft_friendly::stark_252_prime_field::Stark252PrimeField,
            traits::IsFFTField,
        },
        polynomial::Polynomial,
    };

    use crate::{
        air::{
            context::{AirContext, ProofOptions},
            example::simple_fibonacci,
            trace::TraceTable,
        },
        Domain,
    };

    use super::evaluate_polynomial_on_lde_domain;

    pub type FE = FieldElement<Stark252PrimeField>;

    #[test]
    fn test_domain_constructor() {
        let trace = simple_fibonacci::fibonacci_trace([FE::from(1), FE::from(1)], 8);
        let trace_length = trace[0].len();
        let trace_table = TraceTable::new_from_cols(&trace);
        let coset_offset = 3;
        let blowup_factor: usize = 2;

        let context = AirContext {
            options: ProofOptions {
                blowup_factor: blowup_factor as u8,
                fri_number_of_queries: 1,
                coset_offset,
            },
            trace_length,
            trace_columns: trace_table.n_cols,
            transition_degrees: vec![1],
            transition_exemptions: vec![2],
            transition_offsets: vec![0, 1, 2],
            num_transition_constraints: 1,
        };

        let domain = Domain::new(&simple_fibonacci::FibonacciAIR::from(context));
        assert_eq!(domain.blowup_factor, 2);
        assert_eq!(domain.interpolation_domain_size, trace_length);
        assert_eq!(domain.root_order, trace_length.trailing_zeros());
        assert_eq!(
            domain.lde_root_order,
            (trace_length * blowup_factor).trailing_zeros()
        );
        assert_eq!(domain.coset_offset, FieldElement::from(coset_offset));

        let primitive_root = Stark252PrimeField::get_primitive_root_of_unity(
            (trace_length * blowup_factor).trailing_zeros() as u64,
        )
        .unwrap();

        assert_eq!(
            domain.trace_primitive_root,
            primitive_root.pow(blowup_factor)
        );
        for i in 0..(trace_length * blowup_factor) {
            assert_eq!(
                domain.lde_roots_of_unity_coset[i],
                FieldElement::from(coset_offset) * primitive_root.pow(i)
            );
        }
    }

    #[test]
    fn test_evaluate_polynomial_on_lde_domain_on_trace_polys() {
        let trace = simple_fibonacci::fibonacci_trace([FE::from(1), FE::from(1)], 8);
        let trace_length = trace[0].len();
        let trace_table = TraceTable::new_from_cols(&trace);
        let trace_polys = trace_table.compute_trace_polys();
        let coset_offset = FE::from(3);
        let blowup_factor: usize = 2;
        let domain_size = 8;

        let primitive_root = Stark252PrimeField::get_primitive_root_of_unity(
            (trace_length * blowup_factor).trailing_zeros() as u64,
        )
        .unwrap();

        for poly in trace_polys.iter() {
            let lde_evaluation =
                evaluate_polynomial_on_lde_domain(poly, blowup_factor, domain_size, &coset_offset)
                    .unwrap();
            assert_eq!(lde_evaluation.len(), trace_length * blowup_factor);
            for (i, evaluation) in lde_evaluation.iter().enumerate() {
                assert_eq!(
                    *evaluation,
                    poly.evaluate(&(&coset_offset * primitive_root.pow(i)))
                );
            }
        }
    }

    #[test]
    fn test_evaluate_polynomial_on_lde_domain_edge_case() {
        let poly = Polynomial::new_monomial(FE::one(), 8);
        let blowup_factor: usize = 4;
        let domain_size: usize = 8;
        let offset = FE::from(3);
        let evaluations =
            evaluate_polynomial_on_lde_domain(&poly, blowup_factor, domain_size, &offset).unwrap();
        assert_eq!(evaluations.len(), domain_size * blowup_factor);

        let primitive_root: FE = Stark252PrimeField::get_primitive_root_of_unity(
            (domain_size * blowup_factor).trailing_zeros() as u64,
        )
        .unwrap();
        for (i, eval) in evaluations.iter().enumerate() {
            assert_eq!(*eval, poly.evaluate(&(&offset * &primitive_root.pow(i))));
        }
    }
}
