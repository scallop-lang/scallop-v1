use scallop_runtime::{wmc::*, tags::*};
use anyhow::Result;
use tch::{nn, nn::ModuleT, nn::OptimizerConfig, Device, Tensor, index::*, Reduction};
use profiler::Profiler;
use mnist_exp::*;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(name = "sum4-decouple")]
struct Options {
  #[structopt(short = "g", long = "gpu")]
  cuda_gpu: Option<usize>,

  #[structopt(long, default_value = "3")]
  train_k: usize,

  #[structopt(long, default_value = "1")]
  test_k: usize,

  #[structopt(short = "b", long = "batch-size", default_value = "10")]
  batch_size: i64,

  #[structopt(short = "l", long = "learning-rate", default_value = "0.001")]
  learning_rate: f64,

  #[structopt(short = "e", long = "epochs", default_value = "1")]
  num_epochs: usize,

  #[structopt(long = "log-dir", default_value = "log")]
  log_dir: String,

  #[structopt(short = "s", long, default_value = "1234")]
  seed: i64,
}

struct Trainer<const TRK: usize, const TEK: usize> {
  options: Options,
  profiler: Profiler,
  var_store: nn::VarStore,
  dataset: tch::vision::dataset::Dataset,
  model: MnistNet,
  optimizer: nn::Optimizer<nn::Adam>,

  // Configurations
  test_tasks: usize,
  num_possible_digits: usize,
  num_digits_per_task: i64,
  num_facts_per_task: usize,
  out_dim: i64,
  // num_images: i64,
  num_tasks: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
  Train,
  Test,
}

impl<const TRK: usize, const TEK: usize> Trainer<TRK, TEK> {
  fn new(opt: Options) -> Result<Self> {
    let log_dir = &opt.log_dir;
    let batch_size = opt.batch_size;
    let test_tasks = 1000;
    let num_possible_digits = 10;
    let num_digits_per_task = 4;
    let num_facts_per_task = 40;
    let out_dim = 37;
    let learning_rate = opt.learning_rate;
    tch::manual_seed(opt.seed);

    let m = tch::vision::mnist::load_dir("data")?;
    let num_images = m.train_images.size()[0];
    let num_tasks = num_images / num_digits_per_task;

    let file_name = format!("{}/sum_4_decouple_trk_{}_tek_{}_lr_{}_bs_{}_rs_{}.log", log_dir, TRK, TEK, learning_rate, batch_size, opt.seed);
    let mut profiler = Profiler::new(&file_name);
    profiler.start("total_time");

    let dev = match opt.cuda_gpu {
      Some(g) => Device::Cuda(g),
      None => Device::cuda_if_available(),
    };
    let vs = nn::VarStore::new(dev);
    let net = MnistNet::new(&vs.root());
    let optimizer = nn::Adam::default().build(&vs, learning_rate)?;

    Ok(Self {
      options: opt,
      profiler: profiler,
      var_store: vs,
      dataset: m,
      model: net,
      optimizer: optimizer,

      // Configurations
      test_tasks: test_tasks,
      num_possible_digits: num_possible_digits,
      num_digits_per_task: num_digits_per_task,
      num_facts_per_task: num_facts_per_task,
      out_dim: out_dim,
      // num_images: num_images,
      num_tasks: num_tasks,
    })
  }

  fn run(&mut self) {
    for epoch in 1..=self.options.num_epochs {
      let mut ith_batch = 0;
      let mut ith_task = 0;

      for (bimages, blabels) in self.dataset.train_iter(self.options.batch_size * self.num_digits_per_task).shuffle().to_device(self.var_store.device()) {
        let (num_tasks, train_sum_loss, train_sum_acc) = self.run_batch::<TRK>(Phase::Train, bimages, blabels);

        let prev_ith_task = ith_task;

        // Compute loss
        ith_task += num_tasks;
        ith_batch += 1;
        let avg_loss = train_sum_loss / num_tasks as f64;
        let avg_acc = train_sum_acc / num_tasks as f64;
        self.profiler.log(&format!("{}/{}, Batch {}, loss: {}, acc: {}", ith_task, self.num_tasks, ith_batch, avg_loss, avg_acc));

        if prev_ith_task / self.test_tasks < ith_task / self.test_tasks {
          self.test(&format!("Test at Epoch {}, Batch {}", epoch, ith_batch + 1));
        }
      }

      self.test(&format!("Test at Epoch {}", epoch));

      let test_accuracy = self.model.batch_accuracy_for_logits(&self.dataset.test_images, &self.dataset.test_labels, self.var_store.device(), 1024);
      self.profiler.log(&format!("Epoch: {:4} test image recog acc: {:5.2}%", epoch, 100. * test_accuracy));

      self.profiler.log_profile_record();
    }
  }

  fn test(&mut self, msg: &str) {
    let mut sum_tasks = 0;
    let mut sum_acc = 0.0;
    let mut sum_loss = 0.0;
    for (bimages, blabels) in self.dataset.test_iter(self.options.batch_size * self.num_digits_per_task).to_device(self.var_store.device()) {
      let (num_tasks, test_sum_loss, test_sum_acc) = tch::no_grad(|| {
        self.run_batch::<TEK>(Phase::Test, bimages, blabels)
      });
      sum_tasks += num_tasks;
      sum_loss += test_sum_loss;
      sum_acc += test_sum_acc;
    }
    let loss = sum_loss / sum_tasks as f64;
    let acc = sum_acc / sum_tasks as f64;
    self.profiler.log(&format!("{} =-= loss: {}, acc: {}", msg, loss, acc));
  }

  fn run_batch<const K: usize>(&mut self, phase: Phase, bimages: Tensor, blabels: Tensor) -> (usize, f64, f64) {
    let wmc = DiffTopKProbProofsWMC2::<K>;

    self.optimizer.zero_grad();

    // Make prediction on all the digit images in the batch
    self.profiler.start("prediction");
    let raw_y_preds = self.model.forward_t(&bimages, true);
    self.profiler.end("prediction");

    self.profiler.start("datalog_prepare");

    let y_pred_shape = raw_y_preds.size();
    let num_options = y_pred_shape[y_pred_shape.len() - 1];

    // Loop through each pair of images & their predictions
    let batch_num = y_pred_shape[0] / self.num_digits_per_task;

    // First pass, synchronously generate all the labels
    let (data, y_gt_arr) : (Vec<_>, Vec<_>) = (0..batch_num).map(|i| {

      // Get the data
      let first = raw_y_preds.i((i * self.num_digits_per_task, ..));
      let second = raw_y_preds.i((i * self.num_digits_per_task + 1, ..));
      let third = raw_y_preds.i((i * self.num_digits_per_task + 2, ..));
      let forth = raw_y_preds.i((i * self.num_digits_per_task + 3, ..));

      let first_y : i64 = blabels.i(i * self.num_digits_per_task).into();
      let second_y : i64 = blabels.i(i * self.num_digits_per_task + 1).into();
      let third_y : i64 = blabels.i(i * self.num_digits_per_task + 2).into();
      let forth_y : i64 = blabels.i(i * self.num_digits_per_task + 3).into();

      // Compute ground truth
      let y_index : i64 = first_y + second_y + third_y + forth_y;
      let y_slice = self.var_store.root().var_copy("y", &Tensor::of_slice(
        &(0..self.out_dim)
          .map(|i| if i == y_index { 1.0 as f32 } else { 0.0 })
          .collect::<Vec<_>>()
      ).set_requires_grad(false)).set_requires_grad(false);

      ((first, second, third, forth), y_slice)
    }).unzip();

    self.profiler.end("datalog_prepare");

    self.profiler.start("datalog_execute");

    // Execute all the datalog programs and get their contexts + elements
    let tasks = data.into_iter().map(|(first, second, third, forth)| {
      let mut prog = Sum4::<DiffTopKProbProofs<K>>::new();
      prog.digit().insert_diff_disjunction(
        to_digit_disjunction(&first, 0, num_options)
      );
      prog.digit().insert_diff_disjunction(
        to_digit_disjunction(&second, 1, num_options)
      );
      prog.digit().insert_diff_disjunction(
        to_digit_disjunction(&third, 2, num_options)
      );
      prog.digit().insert_diff_disjunction(
        to_digit_disjunction(&forth, 3, num_options)
      );

      prog.run();

      let results = prog.sum_4().complete().into_iter().into_iter().collect::<Vec<_>>();

      (prog.semiring_context().clone(), results)
    }).collect::<Vec<_>>();

    self.profiler.end("datalog_execute");

    self.profiler.start("wmc");

    // Perform parallel wmc
    let results = (0..tasks.len() * self.out_dim as usize).map(|i| {
      let task_id = i / self.out_dim as usize;
      let elem_id = i % self.out_dim as usize;
      let ctx = &tasks[task_id].0;
      let elem = &tasks[task_id].1[elem_id];
      let pred_num = elem.tup;
      let tag = &elem.tag;
      let (prob, derivs) = wmc.wmc(ctx, tag);
      (pred_num, prob, derivs)
    }).collect::<Vec<_>>();

    self.profiler.end("wmc");

    self.profiler.start("loss");

    let (y_preds, loss_and_acc): (Vec<_>, Vec<_>) = (0..tasks.len()).map(|i| {
      let mut probs = vec![0.0; self.out_dim as usize];
      for j in 0..self.out_dim as usize {
        let index = i * self.out_dim as usize + j;
        let result = &results[index];
        probs[result.0 as usize] = result.1;
      }

      let y_pred = self.var_store.root().var_copy("y_p", &Tensor::of_slice(&probs)).set_requires_grad(true);
      let y_gt = &y_gt_arr[i];
      let loss = y_pred.binary_cross_entropy::<&Tensor>(y_gt, None, Reduction::Mean);

      let y_pred_i : i64 = y_pred.argmax(0, false).into();
      let acc = if y_gt.double_value(&[y_pred_i]) >= 0.9 { 1.0 } else { 0.0 };

      if phase == Phase::Train {
        loss.backward();
      }

      (y_pred, (loss.double_value(&[]), acc))
    }).unzip();

    self.profiler.end("loss");

    // Check phase
    if phase == Phase::Train {
      // Backward
      self.profiler.start("backward_1");
      let mut grad_store = (0..tasks.len() * self.num_facts_per_task).map(|_| {
        self.var_store.root().zeros("z", &[]).set_requires_grad(false)
      }).collect::<Vec<_>>();
      (0..tasks.len() as i64 * self.out_dim).for_each(|i| {
        let task_id = i / self.out_dim;
        let elem_id = i % self.out_dim;

        let y_pred = &y_preds[task_id as usize];
        let y_pred_grad_ith = y_pred.grad().i(elem_id);

        let var_id_to_deriv_map = &results[i as usize].2;
        for (fact_id, deriv) in var_id_to_deriv_map {
          let mult_grad = deriv.clone() * &y_pred_grad_ith;
          let index = self.num_facts_per_task * task_id as usize + fact_id;
          grad_store[index] += mult_grad;
        }
      });
      self.profiler.end("backward_1");

      // Backward
      self.profiler.start("backward_2");
      for (i, grad) in grad_store.into_iter().enumerate() {
        let task_id = i / self.num_facts_per_task;
        let fact_id = i % self.num_facts_per_task;

        let row = task_id * self.num_digits_per_task as usize + fact_id / self.num_possible_digits;
        let col = fact_id % self.num_possible_digits;

        raw_y_preds.i((row as i64, col as i64)).backward_with_grad(&grad, true, false);
      }
      self.profiler.end("backward_2");

      // Step
      self.profiler.start("step");
      self.optimizer.step();
      self.profiler.end("step");
    }

    let (sum_loss, sum_acc) = loss_and_acc.iter().fold((0.0, 0.0), |(al, aa), (l, a)| (al + l, aa + a));
    (tasks.len(), sum_loss, sum_acc)
  }
}

fn main() -> Result<()> {
  let opt = Options::from_args();
  if opt.train_k == 3 && opt.test_k == 1 {
    Trainer::<3, 1>::new(opt)?.run()
  } else if opt.train_k == 1 && opt.test_k == 1 {
    Trainer::<1, 1>::new(opt)?.run()
  } else if opt.train_k == 3 && opt.test_k == 3 {
    Trainer::<3, 3>::new(opt)?.run()
  } else if opt.train_k == 5 && opt.test_k == 5 {
    Trainer::<5, 5>::new(opt)?.run()
  } else if opt.train_k == 10 && opt.test_k == 10 {
    Trainer::<10, 10>::new(opt)?.run()
  } else if opt.train_k == 15 && opt.test_k == 15 {
    Trainer::<15, 15>::new(opt)?.run()
  } else if opt.train_k == 20 && opt.test_k == 20 {
    Trainer::<20, 20>::new(opt)?.run()
  } else if opt.train_k == 5 && opt.test_k == 1 {
    Trainer::<5, 1>::new(opt)?.run()
  } else if opt.train_k == 10 && opt.test_k == 1 {
    Trainer::<10, 1>::new(opt)?.run()
  } else if opt.train_k == 15 && opt.test_k == 1 {
    Trainer::<15, 1>::new(opt)?.run()
  } else if opt.train_k == 15 && opt.test_k == 15 {
    Trainer::<15, 15>::new(opt)?.run()
  } else {
    panic!("TRK = {}, TEK = {} not supported", opt.train_k, opt.test_k)
  }
  Ok(())
}
